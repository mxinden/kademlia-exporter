use futures::prelude::*;
use libp2p::{
    core::{
        self, either::EitherError, either::EitherOutput, multiaddr::Protocol,
        muxing::StreamMuxerBox, transport::boxed::Boxed, transport::Transport, upgrade, Multiaddr,
    },
    dns,
    identify::{Identify, IdentifyEvent},
    identity::Keypair,
    kad::{record::store::MemoryStore, Kademlia, KademliaConfig, KademliaEvent},
    mplex, noise,
    ping::{Ping, PingConfig, PingEvent},
    secio,
    swarm::{NetworkBehaviourAction, NetworkBehaviourEventProcess, PollParameters, SwarmBuilder},
    tcp, yamux, InboundUpgradeExt, NetworkBehaviour, OutboundUpgradeExt, PeerId, Swarm,
};
use std::{
    error::Error,
    io,
    pin::Pin,
    task::{Context, Poll},
    time::Duration,
    usize,
};

mod global_only;

pub struct Client {
    swarm: Swarm<MyBehaviour>,
    listening: bool,
}

impl Client {
    pub fn new(mut bootnode: Multiaddr) -> Result<Client, Box<dyn Error>> {
        // Create a random key for ourselves.
        let local_key = Keypair::generate_ed25519();
        let local_peer_id = PeerId::from(local_key.public());

        let behaviour = MyBehaviour::new(local_key.clone())?;
        let transport = build_transport(local_key);
        let mut swarm = SwarmBuilder::new(transport, behaviour, local_peer_id)
            .incoming_connection_limit(10)
            .outgoing_connection_limit(10)
            .build();

        // Listen on all interfaces and whatever port the OS assigns.
        Swarm::listen_on(&mut swarm, "/ip4/0.0.0.0/tcp/0".parse()?)?;

        let bootnode_peer_id = if let Protocol::P2p(hash) = bootnode.pop().unwrap() {
            PeerId::from_multihash(hash).unwrap()
        } else {
            panic!("expected peer id");
        };

        swarm.kademlia.add_address(&bootnode_peer_id, bootnode);
        swarm.kademlia.bootstrap();

        Ok(Client {
            swarm,
            listening: false,
        })
    }

    pub fn get_closest_peers(&mut self, peer_id: PeerId) {
        self.swarm.kademlia.get_closest_peers(peer_id);
    }
}

// TODO: this should be a stream instead.
impl Stream for Client {
    type Item = Event;
    fn poll_next(mut self: Pin<&mut Self>, ctx: &mut Context) -> Poll<Option<Self::Item>> {
        match self.swarm.poll_next_unpin(ctx) {
            Poll::Ready(Some(event)) => return Poll::Ready(Some(event)),
            Poll::Ready(None) => return Poll::Ready(None),
            Poll::Pending => {
                if !self.listening {
                    for listener in Swarm::listeners(&self.swarm) {
                        println!("Swarm listening on {:?}", listener);
                    }
                    self.listening = true;
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
    pub(crate) ping: Ping,
    pub(crate) identify: Identify,

    #[behaviour(ignore)]
    event_buffer: Vec<Event>,
}

#[derive(Debug)]
pub enum Event {
    Ping(PingEvent),
    Identify(Box<IdentifyEvent>),
    Kademlia(Box<KademliaEvent>),
}

impl MyBehaviour {
    fn new(local_key: Keypair) -> Result<Self, Box<dyn Error>> {
        let local_peer_id = PeerId::from(local_key.public());

        // Create a Kademlia behaviour.
        let store = MemoryStore::new(local_peer_id.clone());
        let mut kademlia_config = KademliaConfig::default();
        // TODO: Seems like rust and golang use diffferent max packet sizes
        // https://github.com/libp2p/go-libp2p-core/blob/master/network/network.go#L23
        // https://github.com/libp2p/rust-libp2p/blob/master/protocols/kad/src/protocol.rs#L170
        // This results in `[2020-04-11T22:45:24Z DEBUG libp2p_kad::behaviour]
        // Request to PeerId("") in query QueryId(0) failed with Io(Custom {
        // kind: PermissionDenied, error: "len > max" })`
        kademlia_config.set_max_packet_size(8000);
        let kademlia = Kademlia::with_config(local_peer_id, store, kademlia_config);

        let ping = Ping::new(PingConfig::new().with_keep_alive(true));

        let user_agent = "substrate-node/v2.0.0-e3245d49d-x86_64-linux-gnu (unknown)".to_string();
        let proto_version = "/substrate/1.0".to_string();
        let identify = Identify::new(proto_version, user_agent, local_key.public());

        Ok(MyBehaviour {
            kademlia,
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

impl NetworkBehaviourEventProcess<PingEvent> for MyBehaviour {
    fn inject_event(&mut self, event: PingEvent) {
        self.event_buffer.push(Event::Ping(event));
    }
}

impl NetworkBehaviourEventProcess<IdentifyEvent> for MyBehaviour {
    fn inject_event(&mut self, event: IdentifyEvent) {
        self.event_buffer.push(Event::Identify(Box::new(event)));
    }
}

impl NetworkBehaviourEventProcess<KademliaEvent> for MyBehaviour {
    fn inject_event(&mut self, event: KademliaEvent) {
        self.event_buffer.push(Event::Kademlia(Box::new(event)));
    }
}

fn build_transport(keypair: Keypair) -> Boxed<(PeerId, StreamMuxerBox), impl Error> {
    let tcp = tcp::TcpConfig::new().nodelay(true);
    // Ignore any non global IP addresses. Given the amount of private IP
    // addresses in most Dhts dialing private IP addresses can easily be (and
    // has been) interpreted as a port-scan by ones hosting provider.
    let global_only_tcp = global_only::GlobalIpOnly::new(tcp);
    let transport = dns::DnsConfig::new(global_only_tcp).unwrap();

    let noise_keypair = noise::Keypair::new().into_authentic(&keypair).unwrap();
    let noise_config = noise::NoiseConfig::ix(noise_keypair);
    let secio_config = secio::SecioConfig::new(keypair).max_frame_len(1024 * 1024);

    let transport = transport.and_then(move |stream, endpoint| {
        let upgrade = core::upgrade::SelectUpgrade::new(noise_config, secio_config);
        core::upgrade::apply(stream, upgrade, endpoint, upgrade::Version::V1).map(
            |out| match out? {
                // We negotiated noise
                EitherOutput::First((remote_id, out)) => {
                    let remote_key = match remote_id {
                        noise::RemoteIdentity::IdentityKey(key) => key,
                        _ => {
                            return Err(upgrade::UpgradeError::Apply(EitherError::A(
                                noise::NoiseError::InvalidKey,
                            )))
                        }
                    };
                    Ok((EitherOutput::First(out), remote_key.into_peer_id()))
                }
                // We negotiated secio
                EitherOutput::Second((remote_id, out)) => {
                    Ok((EitherOutput::Second(out), remote_id))
                }
            },
        )
    });

    let mut mplex_config = mplex::MplexConfig::new();
    mplex_config.max_buffer_len_behaviour(mplex::MaxBufferBehaviour::Block);
    mplex_config.max_buffer_len(usize::MAX);
    let yamux_config = yamux::Config::default();

    // Multiplexing
    transport
        .and_then(move |(stream, peer_id), endpoint| {
            let peer_id2 = peer_id.clone();
            let upgrade = core::upgrade::SelectUpgrade::new(yamux_config, mplex_config)
                .map_inbound(move |muxer| (peer_id, muxer))
                .map_outbound(move |muxer| (peer_id2, muxer));

            core::upgrade::apply(stream, upgrade, endpoint, upgrade::Version::V1)
                .map_ok(|(id, muxer)| (id, core::muxing::StreamMuxerBox::new(muxer)))
        })
        .timeout(Duration::from_secs(20))
        .map_err(|err| io::Error::new(io::ErrorKind::Other, err))
        .boxed()
}
