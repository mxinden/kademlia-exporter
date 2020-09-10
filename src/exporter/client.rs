use crate::config::DhtConfig;
use futures::prelude::*;
use libp2p::{
    core::{
        either::{EitherError, EitherOutput},
        self, multiaddr::Protocol, muxing::StreamMuxerBox, transport::boxed::Boxed,
        transport::Transport, upgrade,
    },
    dns,
    identify::{Identify, IdentifyEvent},
    identity::Keypair,
    kad::{record::store::MemoryStore, Kademlia, KademliaConfig, KademliaEvent},
    mplex, noise,
    ping::{Ping, PingConfig, PingEvent},
    swarm::{
        DialError, NetworkBehaviourAction, NetworkBehaviourEventProcess, PollParameters,
        SwarmBuilder,
    },
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
    pub fn new(config: DhtConfig) -> Result<Client, Box<dyn Error>> {
        // Create a random key for ourselves.
        let local_key = Keypair::generate_ed25519();
        let local_peer_id = PeerId::from(local_key.public());

        let behaviour = MyBehaviour::new(
            local_key.clone(),
            config.disjoint_query_paths,
            config.protocol_name,
        )?;
        let transport = build_transport(local_key, config.noise_legacy);
        let mut swarm = SwarmBuilder::new(transport, behaviour, local_peer_id)
            .incoming_connection_limit(100)
            .outgoing_connection_limit(100)
            .build();

        // Listen on all interfaces and whatever port the OS assigns.
        Swarm::listen_on(&mut swarm, "/ip4/0.0.0.0/tcp/0".parse()?)?;

        for mut bootnode in config.bootnodes {
            let bootnode_peer_id = if let Protocol::P2p(hash) = bootnode.pop().unwrap() {
                PeerId::from_multihash(hash).unwrap()
            } else {
                panic!("expected peer id");
            };
            swarm.kademlia.add_address(&bootnode_peer_id, bootnode);
        }

        swarm.kademlia.bootstrap().unwrap();

        Ok(Client {
            swarm,
            listening: false,
        })
    }

    pub fn get_closest_peers(&mut self, peer_id: PeerId) {
        self.swarm.kademlia.get_closest_peers(peer_id);
    }

    pub fn dial(&mut self, peer_id: &PeerId) -> Result<(), DialError> {
        Swarm::dial(&mut self.swarm, peer_id)
    }
}

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
    fn new(
        local_key: Keypair,
        disjoint_query_paths: bool,
        protocol_name: Option<String>,
    ) -> Result<Self, Box<dyn Error>> {
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
        if let Some(protocol_name) = protocol_name {
            kademlia_config.set_protocol_name(protocol_name.into_bytes());
        }
        if disjoint_query_paths {
            kademlia_config.disjoint_query_paths(true);
        }
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

fn build_transport(keypair: Keypair, noise_legacy: bool) -> Boxed<(PeerId, StreamMuxerBox), impl Error> {
    let tcp = tcp::TcpConfig::new().nodelay(true);
    // Ignore any non global IP addresses. Given the amount of private IP
    // addresses in most Dhts dialing private IP addresses can easily be (and
    // has been) interpreted as a port-scan by ones hosting provider.
    let global_only_tcp = global_only::GlobalIpOnly::new(tcp);
    let transport = dns::DnsConfig::new(global_only_tcp).unwrap();

    // Build configuration objects for encryption mechanisms.
    let noise_config = {
        let noise_keypair_legacy = noise::Keypair::<noise::X25519>::new().into_authentic(&keypair)
            .expect("can only fail in case of a hardware bug; since this signing is performed only \
                     once and at initialization, we're taking the bet that the inconvenience of a very \
                     rare panic here is basically zero");
        let noise_keypair_spec = noise::Keypair::<noise::X25519Spec>::new().into_authentic(&keypair)
            .expect("can only fail in case of a hardware bug; since this signing is performed only \
                     once and at initialization, we're taking the bet that the inconvenience of a very \
                     rare panic here is basically zero");

        let mut xx_config = noise::NoiseConfig::xx(noise_keypair_spec);
        let mut ix_config = noise::NoiseConfig::ix(noise_keypair_legacy);
        if noise_legacy {
            // Legacy noise configurations for backward compatibility.
            let mut legacy_config = noise::LegacyConfig::default();
            legacy_config.send_legacy_handshake = true;
            legacy_config.recv_legacy_handshake = true;

            xx_config.set_legacy_config(legacy_config.clone());
            ix_config.set_legacy_config(legacy_config);
        }

        core::upgrade::SelectUpgrade::new(xx_config, ix_config)
    };

    // Encryption
    let transport = transport.and_then(move |stream, endpoint| {
        core::upgrade::apply(stream, noise_config, endpoint, upgrade::Version::V1)
            .map_err(|err|
                     err.map_err(|err| match err {
                         EitherError::A(err) => err,
                         EitherError::B(err) => err,
                     })
            )
            .and_then(|result| async move {
                let remote_key = match &result {
                    EitherOutput::First((noise::RemoteIdentity::IdentityKey(key), _)) => key.clone(),
                    EitherOutput::Second((noise::RemoteIdentity::IdentityKey(key), _)) => key.clone(),
                    _ => return Err(upgrade::UpgradeError::Apply(noise::NoiseError::InvalidKey))
                };
                let out = match result {
                    EitherOutput::First((_, o)) => o,
                    EitherOutput::Second((_, o)) => o,
                };
                Ok((out, remote_key.into_peer_id()))
            })
    });

    let mut mplex_config = mplex::MplexConfig::new();
    mplex_config.max_buffer_len_behaviour(mplex::MaxBufferBehaviour::Block);
    mplex_config.max_buffer_len(usize::MAX);
    let mut yamux_config = libp2p::yamux::Config::default();
    yamux_config.set_window_update_mode(libp2p::yamux::WindowUpdateMode::OnRead);

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
