use futures::prelude::*;
use libp2p::{
    core::{
        self, muxing::StreamMuxerBox, transport::boxed::Boxed, transport::Transport, Multiaddr,
    },
    dns,
    identify::{Identify, IdentifyEvent},
    identity::Keypair,
    kad::{record::store::MemoryStore, Kademlia, KademliaEvent},
    mdns::{Mdns, MdnsEvent},
    noise,
    ping::{Ping, PingConfig, PingEvent},
    swarm::NetworkBehaviourEventProcess,
    tcp, yamux, NetworkBehaviour, PeerId, Swarm,
};
use prometheus::{CounterVec, Gauge, Opts, Registry};
use std::{
    convert::TryInto,
    error::Error,
    pin::Pin,
    str::FromStr,
    task::{Context, Poll},
    time::Duration,
};
pub(crate) struct Exporter {
    swarm: Swarm<MyBehaviour>,
    listening: bool,
}

#[derive(NetworkBehaviour)]
struct MyBehaviour {
    kademlia: Kademlia<MemoryStore>,
    mdns: Mdns,
    ping: Ping,
    identify: Identify,

    #[behaviour(ignore)]
    event_counter: prometheus::CounterVec,
    #[behaviour(ignore)]
    kad_kbuckets_size: prometheus::Gauge,
}

impl NetworkBehaviourEventProcess<MdnsEvent> for MyBehaviour {
    // Called when `mdns` produces an event.
    fn inject_event(&mut self, event: MdnsEvent) {
        match event {
            MdnsEvent::Discovered(list) => {
                self.event_counter
                    .with_label_values(&["mdns", "discovered"])
                    .inc();
                for (peer_id, multiaddr) in list {
                    self.kademlia.add_address(&peer_id, multiaddr);
                }
            }
            MdnsEvent::Expired(_) => {
                self.event_counter
                    .with_label_values(&["mdns", "expired"])
                    .inc();
            }
        }
    }
}

impl NetworkBehaviourEventProcess<PingEvent> for MyBehaviour {
    fn inject_event(&mut self, _event: PingEvent) {
        self.event_counter
            .with_label_values(&["ping", "ping_event"])
            .inc();
    }
}

impl NetworkBehaviourEventProcess<IdentifyEvent> for MyBehaviour {
    fn inject_event(&mut self, event: IdentifyEvent) {
        match event {
            IdentifyEvent::Error { .. } => {
                self.event_counter
                    .with_label_values(&["identify", "error"])
                    .inc();
            }
            IdentifyEvent::Sent { .. } => {
                self.event_counter
                    .with_label_values(&["identify", "sent"])
                    .inc();
            }
            IdentifyEvent::Received { .. } => {
                self.event_counter
                    .with_label_values(&["identify", "received"])
                    .inc();
            }
        }
    }
}

impl NetworkBehaviourEventProcess<KademliaEvent> for MyBehaviour {
    fn inject_event(&mut self, message: KademliaEvent) {
        match message {
            KademliaEvent::BootstrapResult(_) => {
                self.event_counter
                    .with_label_values(&["kad", "bootstrap"])
                    .inc();
            }
            KademliaEvent::GetClosestPeersResult(_) => {
                self.event_counter
                    .with_label_values(&["kad", "get_closest_peers"])
                    .inc();
            }
            KademliaEvent::GetProvidersResult(_) => {
                self.event_counter
                    .with_label_values(&["kad", "get_providers"])
                    .inc();
            }
            KademliaEvent::StartProvidingResult(_) => {
                self.event_counter
                    .with_label_values(&["kad", "start_providing"])
                    .inc();
            }
            KademliaEvent::RepublishProviderResult(_) => {
                self.event_counter
                    .with_label_values(&["kad", "republish_provider"])
                    .inc();
            }
            KademliaEvent::GetRecordResult(_) => {
                self.event_counter
                    .with_label_values(&["kad", "get_record"])
                    .inc();
            }
            KademliaEvent::PutRecordResult(_) => {
                self.event_counter
                    .with_label_values(&["kad", "put_record"])
                    .inc();
            }
            KademliaEvent::RepublishRecordResult(_) => {
                self.event_counter
                    .with_label_values(&["kad", "republish_record"])
                    .inc();
            }
            KademliaEvent::Discovered { .. } => {
                self.event_counter
                    .with_label_values(&["kad", "discovered"])
                    .inc();
            }
            KademliaEvent::RoutingUpdated { old_peer, .. } => {
                // Check if it is a new node, or just an update to a node.
                if old_peer.is_none() {
                    self.kad_kbuckets_size.inc();
                }
                self.event_counter
                    .with_label_values(&["kad", "routing_updated"])
                    .inc();
            }
            KademliaEvent::UnroutablePeer { .. } => {
                self.event_counter
                    .with_label_values(&["kad", "unroutable_peer"])
                    .inc();
            }
        }
    }
}

impl Exporter {
    pub(crate) fn new(registry: &Registry) -> Result<Exporter, Box<dyn Error>> {
        let event_counter = {
            let opts = Opts::new(
                "network_behaviour_event",
                "Libp2p network behaviour events.",
            )
            .variable_labels(vec!["behaviour".to_string(), "event".to_string()]);
            CounterVec::new(opts, &["behaviour", "event"]).unwrap()
        };
        registry.register(Box::new(event_counter.clone())).unwrap();

        let kad_kbuckets_size = {
            let opts = Opts::new("kad_kbuckets_size", "Libp2p Kademlia K-Buckets size.");
            Gauge::with_opts(opts).unwrap()
        };
        registry
            .register(Box::new(kad_kbuckets_size.clone()))
            .unwrap();

        let bootnode: Multiaddr = "/dns4/p2p.cc3-5.kusama.network/tcp/30100"
            .try_into()
            .unwrap();
        let bootnode_peer_id =
            PeerId::from_str("QmdePe9MiAJT4yHT2tEwmazCsckAZb19uaoSUgRDffPq3G").unwrap();

        env_logger::init();

        // Create a random key for ourselves.
        let local_key = Keypair::generate_ed25519();
        let local_peer_id = PeerId::from(local_key.public());

        let transport = build_transport(local_key.clone());

        // Create a swarm to manage peers and events.
        let mut swarm = {
            // Create a Kademlia behaviour.
            let store = MemoryStore::new(local_peer_id.clone());
            let kademlia = Kademlia::new(local_peer_id.clone(), store);
            let mdns = Mdns::new()?;
            let ping = Ping::new(PingConfig::new().with_keep_alive(true));

            let user_agent =
                "substrate-node/v2.0.0-e3245d49d-x86_64-linux-gnu (unknown)".to_string();
            let proto_version = "/substrate/1.0".to_string();
            let identify = Identify::new(proto_version, user_agent, local_key.public());

            let behaviour = MyBehaviour {
                kademlia,
                mdns,
                ping,
                identify,

                // Prometheus metrics
                event_counter,
                kad_kbuckets_size,
            };
            Swarm::new(transport, behaviour, local_peer_id)
        };

        // Listen on all interfaces and whatever port the OS assigns.
        Swarm::listen_on(&mut swarm, "/ip4/0.0.0.0/tcp/0".parse()?)?;

        swarm.kademlia.add_address(&bootnode_peer_id, bootnode);

        swarm.kademlia.bootstrap();

        Ok(Exporter {
            swarm,
            listening: false,
        })
    }
}

impl Future for Exporter {
    type Output = ();
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        loop {
            match self.swarm.poll_next_unpin(cx) {
                Poll::Ready(Some(event)) => println!("{:?}", event),
                Poll::Ready(None) => return Poll::Ready(()),
                Poll::Pending => {
                    if !self.listening {
                        for listener in Swarm::listeners(&self.swarm) {
                            println!("Swarm listening on {:?}", listener);
                        }
                        self.listening = true;
                    }
                    break;
                }
            }
        }
        Poll::Pending
    }
}

fn build_transport(keypair: Keypair) -> Boxed<(PeerId, StreamMuxerBox), impl Error> {
    let tcp = tcp::TcpConfig::new().nodelay(true);
    let transport = dns::DnsConfig::new(tcp).unwrap();

    let noise_keypair = noise::Keypair::new().into_authentic(&keypair).unwrap();

    transport
        .upgrade(core::upgrade::Version::V1)
        .authenticate(noise::NoiseConfig::ix(noise_keypair).into_authenticated())
        .multiplex(yamux::Config::default())
        .map(|(peer, muxer), _| (peer, core::muxing::StreamMuxerBox::new(muxer)))
        .timeout(Duration::from_secs(20))
        .boxed()
}
