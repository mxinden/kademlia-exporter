use crate::config::Config;
use futures::executor::block_on;
use futures::prelude::*;
use futures::ready;
use libp2p::bandwidth::BandwidthSinks;
use libp2p::TransportExt;
use libp2p::{
    core::{
        self, either::EitherOutput, multiaddr::Protocol, muxing::StreamMuxerBox, transport::Boxed,
        transport::Transport, upgrade, Multiaddr,
    },
    dns, identify,
    identity::Keypair,
    kad::{record::store::MemoryStore, Kademlia, KademliaConfig},
    metrics::{Metrics, Recorder},
    mplex, noise, ping, swarm,
    swarm::NetworkBehaviour,
    swarm::{DialError, NetworkInfo, SwarmBuilder, SwarmEvent},
    tcp, yamux, InboundUpgradeExt, OutboundUpgradeExt, PeerId, Swarm,
};
use prometheus_client::registry::Registry;
use std::sync::Arc;
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
    bandwidth_sinks: Arc<BandwidthSinks>,
    metrics: Metrics,
}

impl Client {
    pub fn new(config: Config, registry: &mut Registry) -> Result<Client, Box<dyn Error>> {
        // Create a random key for ourselves.
        let local_key = Keypair::generate_ed25519();
        let local_peer_id = PeerId::from(local_key.public());

        let behaviour = MyBehaviour::new(
            local_key.clone(),
            config.disjoint_query_paths,
            config.protocol_name,
        )?;
        let (transport, bandwidth_sinks) = build_transport(local_key, config.noise_legacy);
        let mut swarm = {
            let mut builder = SwarmBuilder::with_executor(
                transport,
                behaviour,
                local_peer_id,
                Box::new(|fut| {
                    async_std::task::spawn(fut);
                }),
            );
            if let Some(dial_concurrency_factor) = config.dial_concurrency_factor {
                builder = builder.dial_concurrency_factor(dial_concurrency_factor);
            }
            builder.build()
        };

        let tcp_addr = match config.tcp_listen_address {
            Some(addr) => Multiaddr::empty()
                .with(addr.ip().into())
                .with(Protocol::Tcp(addr.port())),
            None => "/ip4/0.0.0.0/tcp/0".parse()?,
        };
        swarm.listen_on(tcp_addr)?;

        let quic_addr = match config.quic_listen_address {
            Some(addr) => Multiaddr::empty()
                .with(addr.ip().into())
                .with(Protocol::Udp(addr.port()))
                .with(Protocol::QuicV1),
            None => "/ip4/0.0.0.0/udp/0/quic-v1".parse()?,
        };
        swarm.listen_on(quic_addr)?;

        for mut bootnode in config.bootnodes {
            let bootnode_peer_id = if let Protocol::P2p(hash) = bootnode.pop().unwrap() {
                PeerId::from_multihash(hash).unwrap()
            } else {
                panic!("expected peer id");
            };
            swarm
                .behaviour_mut()
                .kademlia
                .add_address(&bootnode_peer_id, bootnode);
        }

        swarm.behaviour_mut().kademlia.bootstrap().unwrap();

        Ok(Client {
            swarm,
            bandwidth_sinks,
            metrics: Metrics::new(registry),
        })
    }

    pub fn get_closest_peers(&mut self, peer_id: PeerId) {
        self.swarm
            .behaviour_mut()
            .kademlia
            .get_closest_peers(peer_id);
    }

    pub fn dial(&mut self, peer_id: &PeerId) -> Result<bool, DialError> {
        if Swarm::is_connected(&mut self.swarm, peer_id) {
            Ok(false)
        } else {
            self.swarm.dial(*peer_id)?;
            Ok(true)
        }
    }

    pub fn network_info(&self) -> NetworkInfo {
        Swarm::network_info(&self.swarm)
    }

    pub fn total_outbound(&self) -> u64 {
        self.bandwidth_sinks.total_outbound()
    }

    pub fn total_inbound(&self) -> u64 {
        self.bandwidth_sinks.total_inbound()
    }
}

impl Stream for Client {
    type Item = ClientEvent;
    fn poll_next(mut self: Pin<&mut Self>, ctx: &mut Context) -> Poll<Option<Self::Item>> {
        loop {
            let event = ready!(self.swarm.poll_next_unpin(ctx)).expect("Infinite stream.");
            self.metrics.record(&event);

            match event {
                SwarmEvent::Behaviour(event) => {
                    match &event {
                        MyBehaviourEvent::Ping(e) => self.metrics.record(e),
                        MyBehaviourEvent::Identify(e) => self.metrics.record(e),
                        MyBehaviourEvent::Kademlia(e) => self.metrics.record(e),
                        MyBehaviourEvent::KeepAlive(v) => void::unreachable(*v),
                    }
                    return Poll::Ready(Some(ClientEvent::Behaviour(event)));
                }
                SwarmEvent::NewListenAddr { address, .. } => {
                    println!("Swarm listening on {:?}", address);
                }
                SwarmEvent::ConnectionClosed {
                    peer_id,
                    num_established,
                    ..
                } if num_established == 0 => {
                    return Poll::Ready(Some(ClientEvent::AllConnectionsClosed(peer_id)));
                }
                _ => {}
            }
        }
    }
}

#[derive(Debug)]
pub enum ClientEvent {
    Behaviour(MyBehaviourEvent),
    AllConnectionsClosed(PeerId),
}

#[derive(NetworkBehaviour)]
pub struct MyBehaviour {
    pub(crate) kademlia: Kademlia<MemoryStore>,
    pub(crate) ping: ping::Behaviour,
    pub(crate) identify: identify::Behaviour,
    keep_alive: swarm::keep_alive::Behaviour,
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
            kademlia_config.set_protocol_names(vec![protocol_name.into_bytes().into()]);
        }

        if disjoint_query_paths {
            kademlia_config.disjoint_query_paths(true);
        }

        // Instantly remove records and provider records.
        //
        // TODO: Replace hack with option to disable both.
        kademlia_config.set_record_ttl(Some(Duration::from_secs(0)));
        kademlia_config.set_provider_record_ttl(Some(Duration::from_secs(0)));

        let kademlia = Kademlia::with_config(local_peer_id, store, kademlia_config);

        let ping = ping::Behaviour::new(ping::Config::new());

        let proto_version = "/libp2p/1.0.0".to_string();
        let identify = identify::Behaviour::new(
            identify::Config::new(proto_version, local_key.public())
                .with_agent_version(format!("rust-libp2p/{}", env!("CARGO_PKG_VERSION"))),
        );

        Ok(MyBehaviour {
            kademlia,
            ping,
            identify,
            keep_alive: swarm::keep_alive::Behaviour,
        })
    }
}

fn build_transport(
    keypair: Keypair,
    noise_legacy: bool,
) -> (Boxed<(PeerId, StreamMuxerBox)>, Arc<BandwidthSinks>) {
    let tcp = tcp::async_io::Transport::new(tcp::Config::default().nodelay(true));
    // Ignore any non global IP addresses. Given the amount of private IP
    // addresses in most Dhts dialing private IP addresses can easily be (and
    // has been) interpreted as a port-scan by ones hosting provider.
    let global_only_tcp = global_only::GlobalIpOnly::new(tcp);
    let transport = block_on(dns::DnsConfig::system(global_only_tcp)).unwrap();
    let (transport, bandwidth_sinks) = transport.with_bandwidth_logging();

    let authentication_config = {
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
            let mut noise_legacy = noise::LegacyConfig::default();
            noise_legacy.recv_legacy_handshake = true;

            xx_config.set_legacy_config(noise_legacy.clone());
            ix_config.set_legacy_config(noise_legacy);
        }

        let extract_peer_id = |result| match result {
            EitherOutput::First((peer_id, o)) => (peer_id, EitherOutput::First(o)),
            EitherOutput::Second((peer_id, o)) => (peer_id, EitherOutput::Second(o)),
        };

        core::upgrade::SelectUpgrade::new(
            xx_config.into_authenticated(),
            ix_config.into_authenticated(),
        )
        .map_inbound(extract_peer_id)
        .map_outbound(extract_peer_id)
    };

    let multiplexing_config = {
        let mut mplex_config = mplex::MplexConfig::new();
        mplex_config.set_max_buffer_behaviour(mplex::MaxBufferBehaviour::Block);
        mplex_config.set_max_buffer_size(usize::MAX);

        let mut yamux_config = yamux::YamuxConfig::default();
        // Enable proper flow-control: window updates are only sent when
        // buffered data has been consumed.
        yamux_config.set_window_update_mode(yamux::WindowUpdateMode::on_read());

        core::upgrade::SelectUpgrade::new(yamux_config, mplex_config)
            .map_inbound(move |muxer| core::muxing::StreamMuxerBox::new(muxer))
            .map_outbound(move |muxer| core::muxing::StreamMuxerBox::new(muxer))
    };

    let quic_transport = {
        let config = libp2p::quic::Config::new(&keypair);
        // TODO: Support DNS.
        libp2p::quic::async_std::Transport::new(config)
    };

    let transport = libp2p::core::transport::OrTransport::new(
        quic_transport,
        transport
            .upgrade(upgrade::Version::V1Lazy)
            .authenticate(authentication_config)
            .multiplex(multiplexing_config)
            .timeout(Duration::from_secs(20)),
    )
    .map(|either_output, _| match either_output {
        EitherOutput::First((peer_id, muxer)) => (peer_id, StreamMuxerBox::new(muxer)),
        EitherOutput::Second((peer_id, muxer)) => (peer_id, StreamMuxerBox::new(muxer)),
    })
    .map_err(|err| io::Error::new(io::ErrorKind::Other, err))
    .boxed();
    (transport, bandwidth_sinks)
}
