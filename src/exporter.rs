use crate::behaviour::MyBehaviour;
use futures::prelude::*;
use libp2p::{
    core::{
        self, muxing::StreamMuxerBox, transport::boxed::Boxed, transport::Transport, Multiaddr,
    },
    dns,
    identify::Identify,
    identity::Keypair,
    kad::{record::store::MemoryStore, Kademlia},
    mdns::Mdns,
    noise,
    ping::{Ping, PingConfig},
    tcp, yamux, PeerId, Swarm,
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
