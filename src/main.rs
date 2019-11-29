// TODO: Continue implementing fake event

use async_std::{io, task};
use futures::prelude::*;
use futures::future::{self, Ready};
use libp2p::core::{upgrade::Negotiated, InboundUpgrade, OutboundUpgrade, UpgradeInfo};
use libp2p::kad::record::store::MemoryStore;
use libp2p::kad::{record::Key, Kademlia, KademliaEvent, PutRecordOk, Quorum, Record};
use libp2p::{
    build_development_transport,
    core::muxing::StreamMuxerBox,
    core::transport::boxed::Boxed,
    core::{self, either::EitherError, either::EitherOutput, transport::Transport, upgrade},
    dns, identity,
    mdns::{Mdns, MdnsEvent},
    mplex, noise,
    swarm::NetworkBehaviourEventProcess,
    tcp, websocket, yamux, NetworkBehaviour, PeerId, Swarm,
};
use std::{
    error::Error,
    task::{Context, Poll},
    time::Duration,
    usize,
};
use void::Void;

fn main() -> Result<(), Box<dyn Error>> {
    env_logger::init();

    // Create a random key for ourselves.
    let local_key = identity::Keypair::generate_ed25519();
    let local_peer_id = PeerId::from(local_key.public());

    println!("local peer id: {:?}", local_peer_id);

    // Set up a an encrypted DNS-enabled TCP Transport over the Mplex protocol.
    let transport = build_transport(local_key);

    // We create a custom network behaviour that combines Kademlia and mDNS.
    #[derive(NetworkBehaviour)]
    struct MyBehaviour<TSubstream: AsyncRead + AsyncWrite> {
        kademlia: Kademlia<TSubstream, MemoryStore>,
        mdns: Mdns<TSubstream>,
        fake: FakeConfig,
    }

    impl<T> NetworkBehaviourEventProcess<MdnsEvent> for MyBehaviour<T>
    where
        T: AsyncRead + AsyncWrite,
    {
        // Called when `mdns` produces an event.
        fn inject_event(&mut self, event: MdnsEvent) {
            println!("event: {:?}", event);
            if let MdnsEvent::Discovered(list) = event {
                println!("discovered: {:?}", list.size_hint());
                for (peer_id, multiaddr) in list {
                    println!("discovered: '{:?}'", peer_id);
                    self.kademlia.add_address(&peer_id, multiaddr);
                }
            }
        }
    }

    impl<T> NetworkBehaviourEventProcess<KademliaEvent> for MyBehaviour<T>
    where
        T: AsyncRead + AsyncWrite,
    {
        // Called when `kademlia` produces an event.
        fn inject_event(&mut self, message: KademliaEvent) {
            match message {
                KademliaEvent::GetRecordResult(Ok(result)) => {
                    for Record { key, value, .. } in result.records {
                        println!(
                            "Got record {:?} {:?}",
                            std::str::from_utf8(key.as_ref()).unwrap(),
                            std::str::from_utf8(&value).unwrap(),
                        );
                    }
                }
                KademliaEvent::GetRecordResult(Err(err)) => {
                    eprintln!("Failed to get record: {:?}", err);
                }
                KademliaEvent::PutRecordResult(Ok(PutRecordOk { key })) => {
                    println!(
                        "Successfully put record {:?}",
                        std::str::from_utf8(key.as_ref()).unwrap()
                    );
                }
                KademliaEvent::PutRecordResult(Err(err)) => {
                    eprintln!("Failed to put record: {:?}", err);
                }
                _ => {}
            }
        }
    }

    // Create a swarm to manage peers and events.
    let mut swarm = {
        // Create a Kademlia behaviour.
        let store = MemoryStore::new(local_peer_id.clone());
        let kademlia = Kademlia::new(local_peer_id.clone(), store);
        let mdns = task::block_on(Mdns::new())?;
        let fake = FakeConfig{};
        let behaviour = MyBehaviour { kademlia, mdns, fake };
        Swarm::new(transport, behaviour, local_peer_id)
    };

    // Read full lines from stdin
    let mut stdin = io::BufReader::new(io::stdin()).lines();

    // Listen on all interfaces and whatever port the OS assigns.
    Swarm::listen_on(&mut swarm, "/ip4/127.0.0.1/tcp/0".parse()?)?;

    // Kick it off.
    let mut listening = false;
    task::block_on(future::poll_fn(move |cx: &mut Context| {
        loop {
            match stdin.try_poll_next_unpin(cx)? {
                Poll::Ready(Some(line)) => handle_input_line(&mut swarm.kademlia, line),
                Poll::Ready(None) => panic!("Stdin closed"),
                Poll::Pending => break,
            }
        }
        loop {
            match swarm.poll_next_unpin(cx) {
                Poll::Ready(Some(event)) => println!("{:?}", event),
                Poll::Ready(None) => return Poll::Ready(Ok(())),
                Poll::Pending => {
                    if !listening {
                        if let Some(a) = Swarm::listeners(&swarm).next() {
                            println!("Listening on {:?}", a);
                            listening = true;
                        }
                    }
                    break;
                }
            }
        }
        Poll::Pending
    }))
}

fn handle_input_line<T>(kademlia: &mut Kademlia<T, MemoryStore>, line: String)
where
    T: AsyncRead + AsyncWrite,
{
    let mut args = line.split(" ");

    match args.next() {
        Some("GET") => {
            let key = {
                match args.next() {
                    Some(key) => Key::new(&key),
                    None => {
                        eprintln!("Expected key");
                        return;
                    }
                }
            };
            kademlia.get_record(&key, Quorum::One);
        }
        Some("PUT") => {
            let key = {
                match args.next() {
                    Some(key) => Key::new(&key),
                    None => {
                        eprintln!("Expected key");
                        return;
                    }
                }
            };
            let value = {
                match args.next() {
                    Some(value) => value.as_bytes().to_vec(),
                    None => {
                        eprintln!("Expected value");
                        return;
                    }
                }
            };
            let record = Record {
                key,
                value,
                publisher: None,
                expires: None,
            };
            kademlia.put_record(record, Quorum::One);
        }
        _ => {
            eprintln!("expected GET or PUT");
        }
    }
}

fn build_transport(keypair: identity::Keypair) -> Boxed<(PeerId, StreamMuxerBox), impl Error> {
    let tcp = tcp::TcpConfig::new().nodelay(true);
    let transport = dns::DnsConfig::new(tcp).unwrap();
    // TODO: Do I need:
    // #[cfg(feature = "libp2p-websocket")]
    // let transport = {
    //     let trans_clone = transport.clone();
    //     transport.or_transport(websocket::WsConfig::new(trans_clone))
    // };

    let noise_keypair = noise::Keypair::new().into_authentic(&keypair).unwrap();

    transport
        .upgrade(core::upgrade::Version::V1)
        .authenticate(noise::NoiseConfig::ix(noise_keypair).into_authenticated())
        .multiplex(yamux::Config::default())
        .map(|(peer, muxer), _| (peer, core::muxing::StreamMuxerBox::new(muxer)))
        .timeout(Duration::from_secs(20))
        .boxed()

    //     let noise_config =
    //         {
    //             let noise_keypair = noise::Keypair::new().into_authentic(&keypair)
    // 			  // For more information about this panic, see in "On the Importance of Checking
    // 			  // Cryptographic Protocols for Faults" by Dan Boneh, Richard A. DeMillo,
    // 			  // and Richard J. Lipton.
    // 			      .expect("can only fail in case of a hardware bug; since this signing is performed only \
    // 				             once and at initialization, we're taking the bet that the inconvenience of a very \
    // 				             rare panic here is basically zero");
    //             noise::NoiseConfig::ix(noise_keypair)
    //         };
    //
    //     // Build configuration objects for multiplexing mechanisms.
    //     let mut mplex_config = mplex::MplexConfig::new();
    //     mplex_config.max_buffer_len_behaviour(mplex::MaxBufferBehaviour::Block);
    //     mplex_config.max_buffer_len(usize::MAX);
    //     let yamux_config = yamux::Config::default();
    //
    //     #[cfg(not(target_os = "unknown"))]
    //     let transport = {
    //         let transport = tcp::TcpConfig::new();
    //         let transport = websocket::WsConfig::new(transport.clone()).or_transport(transport);
    //         dns::DnsConfig::new(transport)
    //     };
    //
    //     // For non-WASM, we support both secio and noise.
    //     #[cfg(not(target_os = "unknown"))]
    //     let transport = transport.unwrap().and_then(move |stream, endpoint| {
    //         core::upgrade::apply(stream, noise_config, endpoint, upgrade::Version::V1).and_then(
    //             |(remote_id, out)| {
    //                 let remote_key = match remote_id {
    //                     noise::RemoteIdentity::IdentityKey(key) => key,
    //                     _ => {
    //                         panic!();
    //                         // return Err(upgrade::UpgradeError::Apply(
    //                         //     noise::NoiseError::InvalidKey,
    //                         // ))
    //                     }
    //                 };
    //                 Ok((out, remote_key.into_peer_id()))
    //             },
    //         )
    //     });
    //
    //     // Multiplexing
    //     let transport = transport
    //         .and_then(move |(stream, peer_id), endpoint| {
    //             let peer_id2 = peer_id.clone();
    //             let upgrade = core::upgrade::SelectUpgrade::new(yamux_config, mplex_config)
    //                 .map_inbound(move |muxer| (peer_id, muxer))
    //                 .map_outbound(move |muxer| (peer_id2, muxer));
    //
    //             core::upgrade::apply(stream, upgrade, endpoint, upgrade::Version::V1)
    //                 .map(|(id, muxer)| (id, core::muxing::StreamMuxerBox::new(muxer)))
    //         })
    //         .timeout(Duration::from_secs(20))
    //         .map_err(|err| io::Error::new(io::ErrorKind::Other, err))
    //         .boxed();
    //
    //     return transport;
}

#[derive(Debug, Copy, Clone)]
pub struct FakeConfig;

impl UpgradeInfo for FakeConfig {
    type Info = &'static [u8];
    type InfoIter = std::iter::Once<Self::Info>;

    fn protocol_info(&self) -> Self::InfoIter {
        println!("protocol info called");
        std::iter::once(b"/plaintext/1.0.0")
    }
}

impl<C> InboundUpgrade<C> for FakeConfig {
    type Output = Negotiated<C>;
    type Error = Void;
    type Future = Ready<Result<Negotiated<C>, Self::Error>>;

    fn upgrade_inbound(self, i: Negotiated<C>, _: Self::Info) -> Self::Future {
        println!("upgrade inbound called");
        future::ready(Ok(i))
    }
}

impl<C> OutboundUpgrade<C> for FakeConfig {
    type Output = Negotiated<C>;
    type Error = Void;
    type Future = Ready<Result<Negotiated<C>, Self::Error>>;

    fn upgrade_outbound(self, i: Negotiated<C>, _: Self::Info) -> Self::Future {
        println!("upgrade outbound called");
        future::ready(Ok(i))
    }
}

impl NetworkBehaviour for FakeConfig {
    /// Handler for all the protocols the network behaviour supports.
    type ProtocolsHandler: FakeHandler;

    /// Event generated by the `NetworkBehaviour` and that the swarm will report back.
    type OutEvent: FakeEvent;

    /// Creates a new `ProtocolsHandler` for a connection with a peer.
    ///
    /// Every time an incoming connection is opened, and every time we start dialing a node, this
    /// method is called.
    ///
    /// The returned object is a handler for that specific connection, and will be moved to a
    /// background task dedicated to that connection.
    ///
    /// The network behaviour (ie. the implementation of this trait) and the handlers it has
    /// spawned (ie. the objects returned by `new_handler`) can communicate by passing messages.
    /// Messages sent from the handler to the behaviour are injected with `inject_node_event`, and
    /// the behaviour can send a message to the handler by making `poll` return `SendEvent`.
    fn new_handler(&mut self) -> Self::ProtocolsHandler {
        FakeHandler{}
    }

    /// Addresses that this behaviour is aware of for this specific peer, and that may allow
    /// reaching the peer.
    ///
    /// The addresses will be tried in the order returned by this function, which means that they
    /// should be ordered by decreasing likelihood of reachability. In other words, the first
    /// address should be the most likely to be reachable.
    fn addresses_of_peer(&mut self, peer_id: &PeerId) -> Vec<Multiaddr>{ vec![]}

    /// Indicates the behaviour that we connected to the node with the given peer id through the
    /// given endpoint.
    ///
    /// This node now has a handler (as spawned by `new_handler`) running in the background.
    fn inject_connected(&mut self, peer_id: PeerId, endpoint: ConnectedPoint){}

    /// Indicates the behaviour that we disconnected from the node with the given peer id. The
    /// endpoint is the one we used to be connected to.
    ///
    /// There is no handler running anymore for this node. Any event that has been sent to it may
    /// or may not have been processed by the handler.
    fn inject_disconnected(&mut self, peer_id: &PeerId, endpoint: ConnectedPoint){}

    /// Indicates the behaviour that we replace the connection from the node with another.
    ///
    /// The handler that used to be dedicated to this node has been destroyed and replaced with a
    /// new one. Any event that has been sent to it may or may not have been processed.
    ///
    /// The default implementation of this method calls `inject_disconnected` followed with
    /// `inject_connected`. This is a logically safe way to implement this behaviour. However, you
    /// may want to overwrite this method in the situations where this isn't appropriate.
    fn inject_replaced(&mut self, peer_id: PeerId, closed_endpoint: ConnectedPoint, new_endpoint: ConnectedPoint) {
        self.inject_disconnected(&peer_id, closed_endpoint);
        self.inject_connected(peer_id, new_endpoint);
    }

    /// Informs the behaviour about an event generated by the handler dedicated to the peer identified by `peer_id`.
    /// for the behaviour.
    ///
    /// The `peer_id` is guaranteed to be in a connected state. In other words, `inject_connected`
    /// has previously been called with this `PeerId`.
    fn inject_node_event(
        &mut self,
        peer_id: PeerId,
        event: <<Self::ProtocolsHandler as IntoProtocolsHandler>::Handler as ProtocolsHandler>::OutEvent
    ){}

    /// Indicates to the behaviour that we tried to reach an address, but failed.
    ///
    /// If we were trying to reach a specific node, its ID is passed as parameter. If this is the
    /// last address to attempt for the given node, then `inject_dial_failure` is called afterwards.
    fn inject_addr_reach_failure(&mut self, _peer_id: Option<&PeerId>, _addr: &Multiaddr, _error: &dyn error::Error) {
    }

    /// Indicates to the behaviour that we tried to dial all the addresses known for a node, but
    /// failed.
    ///
    /// The `peer_id` is guaranteed to be in a disconnected state. In other words,
    /// `inject_connected` has not been called, or `inject_disconnected` has been called since then.
    fn inject_dial_failure(&mut self, _peer_id: &PeerId) {
    }

    /// Indicates to the behaviour that we have started listening on a new multiaddr.
    fn inject_new_listen_addr(&mut self, _addr: &Multiaddr) {
    }

    /// Indicates to the behaviour that a new multiaddr we were listening on has expired,
    /// which means that we are no longer listening in it.
    fn inject_expired_listen_addr(&mut self, _addr: &Multiaddr) {
    }

    /// Indicates to the behaviour that we have discovered a new external address for us.
    fn inject_new_external_addr(&mut self, _addr: &Multiaddr) {
    }

    /// A listener experienced an error.
    fn inject_listener_error(&mut self, _id: ListenerId, _err: &(dyn std::error::Error + 'static)) {
    }

    /// A listener closed.
    fn inject_listener_closed(&mut self, _id: ListenerId) {
    }

    /// Polls for things that swarm should do.
    ///
    /// This API mimics the API of the `Stream` trait. The method may register the current task in
    /// order to wake it up at a later point in time.
    fn poll(&mut self, cx: &mut Context, params: &mut impl PollParameters)
        -> Poll<NetworkBehaviourAction<<<Self::ProtocolsHandler as IntoProtocolsHandler>::Handler as ProtocolsHandler>::InEvent, Self::OutEvent>>;
    {
        Poll::Ready(())
    }
}
