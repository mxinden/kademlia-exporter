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
    swarm::{NetworkBehaviourAction, NetworkBehaviourEventProcess, PollParameters},
    tcp, yamux, NetworkBehaviour, PeerId, Swarm,
};
use std::{
    convert::TryInto,
    error::Error,
    pin::Pin,
    str::FromStr,
    task::{Context, Poll},
    time::Duration,
};

pub struct Client {
    swarm: Swarm<MyBehaviour>,
    listening: bool,
}

impl Client {
    pub fn new() -> Result<Client, Box<dyn Error>> {
        env_logger::init();

        // Create a random key for ourselves.
        let local_key = Keypair::generate_ed25519();
        let local_peer_id = PeerId::from(local_key.public());

        let behaviour = MyBehaviour::new(local_key.clone())?;
        let transport = build_transport(local_key);
        let mut swarm = Swarm::new(transport, behaviour, local_peer_id);

        // Listen on all interfaces and whatever port the OS assigns.
        Swarm::listen_on(&mut swarm, "/ip4/0.0.0.0/tcp/0".parse()?)?;

        // Bootstrapping.
        let bootnode: Multiaddr = "/dns4/p2p.cc3-5.kusama.network/tcp/30100"
            .try_into()
            .unwrap();
        let bootnode_peer_id =
            PeerId::from_str("QmdePe9MiAJT4yHT2tEwmazCsckAZb19uaoSUgRDffPq3G").unwrap();
        swarm.kademlia.add_address(&bootnode_peer_id, bootnode);
        swarm.kademlia.bootstrap();

        Ok(Client {
            swarm,
            listening: false,
        })
    }
}

// TODO: this should be a stream instead.
impl Stream for Client {
    type Item = Event;
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Option<Self::Item>> {
        loop {
            match self.swarm.poll_next_unpin(cx) {
                Poll::Ready(Some(event)) => return Poll::Ready(Some(event)),
                Poll::Ready(None) => return Poll::Ready(None),
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

#[derive(NetworkBehaviour)]
#[behaviour(out_event = "Event", poll_method = "poll")]
pub(crate) struct MyBehaviour {
    pub(crate) kademlia: Kademlia<MemoryStore>,
    pub(crate) mdns: Mdns,
    pub(crate) ping: Ping,
    pub(crate) identify: Identify,

    #[behaviour(ignore)]
    event_buffer: Vec<Event>,
}

pub enum Event {
    Mdns(MdnsEvent),
    Ping(PingEvent),
    Identify(IdentifyEvent),
    Kademlia(KademliaEvent),
}

impl MyBehaviour {
    fn new(local_key: Keypair) -> Result<Self, Box<dyn Error>> {
        let local_peer_id = PeerId::from(local_key.public());
        // Create a Kademlia behaviour.
        let store = MemoryStore::new(local_peer_id.clone());
        let kademlia = Kademlia::new(local_peer_id.clone(), store);
        let mdns = Mdns::new()?;
        let ping = Ping::new(PingConfig::new().with_keep_alive(true));

        let user_agent = "substrate-node/v2.0.0-e3245d49d-x86_64-linux-gnu (unknown)".to_string();
        let proto_version = "/substrate/1.0".to_string();
        let identify = Identify::new(proto_version, user_agent, local_key.public());

        Ok(MyBehaviour {
            kademlia,
            mdns,
            ping,
            identify,

            event_buffer: Vec::new(),
        })
    }
    fn poll<TEv>(
        &mut self,
        _: &mut Context,
        _: &mut impl PollParameters,
    ) -> Poll<NetworkBehaviourAction<TEv, Event>> {
        if !self.event_buffer.is_empty() {
            return Poll::Ready(NetworkBehaviourAction::GenerateEvent(
                self.event_buffer.remove(0),
            ));
        }

        Poll::Pending
    }
}

impl NetworkBehaviourEventProcess<MdnsEvent> for MyBehaviour {
    // Called when `mdns` produces an event.
    fn inject_event(&mut self, mut event: MdnsEvent) {
        if let MdnsEvent::Discovered(list) = &mut event {
            for (peer_id, multiaddr) in list {
                self.kademlia.add_address(&peer_id, multiaddr);
            }
        }
        self.event_buffer.push(Event::Mdns(event));
    }
}

impl NetworkBehaviourEventProcess<PingEvent> for MyBehaviour {
    fn inject_event(&mut self, event: PingEvent) {
        self.event_buffer.push(Event::Ping(event));
    }
}

impl NetworkBehaviourEventProcess<IdentifyEvent> for MyBehaviour {
    fn inject_event(&mut self, event: IdentifyEvent) {
        self.event_buffer.push(Event::Identify(event));
    }
}

impl NetworkBehaviourEventProcess<KademliaEvent> for MyBehaviour {
    fn inject_event(&mut self, event: KademliaEvent) {
        self.event_buffer.push(Event::Kademlia(event));
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
