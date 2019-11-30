// TODO: Continue implementing fake event

use async_std::{io, task};
use futures::future::{self, Ready};
use futures::prelude::*;
use libp2p::core::{
    nodes::ListenerId, upgrade::DeniedUpgrade, upgrade::Negotiated, ConnectedPoint, InboundUpgrade,
    Multiaddr, OutboundUpgrade, UpgradeInfo,
};
use libp2p::kad::record::store::MemoryStore;
use libp2p::kad::{record::Key, Kademlia, KademliaEvent, PutRecordOk, Quorum, Record};
use libp2p::{
    build_development_transport,
    core::muxing::StreamMuxerBox,
    core::transport::boxed::Boxed,
    core::{self, either::EitherError, either::EitherOutput, transport::Transport, upgrade},
    dns,
    identify::{Identify, IdentifyEvent, IdentifyInfo},
    identity,
    mdns::{Mdns, MdnsEvent},
    mplex, noise,
    ping::{Ping, PingConfig, PingEvent},
    swarm::IntoProtocolsHandler,
    swarm::KeepAlive,
    swarm::NetworkBehaviourAction,
    swarm::NetworkBehaviourEventProcess,
    swarm::PollParameters,
    swarm::ProtocolsHandler,
    swarm::ProtocolsHandlerEvent,
    swarm::ProtocolsHandlerUpgrErr,
    swarm::SubstreamProtocol,
    tcp, websocket, yamux, NetworkBehaviour, PeerId, Swarm,
};
use std::{
    error::Error,
    marker::PhantomData,
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
    let transport = build_transport(local_key.clone());

    // We create a custom network behaviour that combines Kademlia and mDNS.
    #[derive(NetworkBehaviour)]
    struct MyBehaviour<TSubstream: AsyncRead + AsyncWrite> {
        kademlia: Kademlia<TSubstream, MemoryStore>,
        mdns: Mdns<TSubstream>,
        fake: FakeConfig<TSubstream>,
        ping: Ping<TSubstream>,
        identify: Identify<TSubstream>,
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

    impl<T> NetworkBehaviourEventProcess<FakeEvent> for MyBehaviour<T>
    where
        T: AsyncRead + AsyncWrite,
    {
        fn inject_event(&mut self, event: FakeEvent) {
            println!("GOT FAKE EVENT");
        }
    }

    impl<T> NetworkBehaviourEventProcess<PingEvent> for MyBehaviour<T>
    where
        T: AsyncRead + AsyncWrite,
    {
        fn inject_event(&mut self, event: PingEvent) {
            println!("Got PingEvent");
        }
    }

    impl<T> NetworkBehaviourEventProcess<IdentifyEvent> for MyBehaviour<T>
    where
        T: AsyncRead + AsyncWrite,
    {
        fn inject_event(&mut self, event: IdentifyEvent) {
            println!("Got IdentifyEvent");
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
                e => println!("Kademlia Event: {:?}", e)
            }
        }
    }

    // Create a swarm to manage peers and events.
    let mut swarm = {
        // Create a Kademlia behaviour.
        let store = MemoryStore::new(local_peer_id.clone());
        let kademlia = Kademlia::new(local_peer_id.clone(), store);
        let mdns = task::block_on(Mdns::new())?;
        let fake = FakeConfig {
            marker: PhantomData,
        };
        let ping = Ping::new(PingConfig::new().with_keep_alive(true));

        let user_agent = "substrate-node/v2.0.0-e3245d49d-x86_64-linux-gnu (unknown)".to_string();
        let proto_version = "/substrate/1.0".to_string();
        let identify = Identify::new(proto_version, user_agent, local_key.public());

        let behaviour = MyBehaviour {
            kademlia,
            mdns,
            fake,
            ping,
            identify,
        };
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
    for id in kademlia.kbuckets_entries() {
        println!("Connected to: {:?}", id);
    }

    kademlia.bootstrap();

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
pub struct FakeConfig<TSubstream> {
    /// Marker to pin the generic.
    marker: PhantomData<TSubstream>,
}

impl UpgradeInfo for FakeProtocolConfig {
    type Info = &'static [u8];
    type InfoIter = Vec<&'static [u8]>;

    fn protocol_info(&self) -> Self::InfoIter {
        // println!("protocol info called");
        vec![b"/substrate/fir/5"]
    }
}

impl<C> InboundUpgrade<C> for FakeProtocolConfig {
    type Output = Negotiated<C>;
    type Error = Void;
    type Future = Ready<Result<Negotiated<C>, Self::Error>>;

    fn upgrade_inbound(self, i: Negotiated<C>, _: Self::Info) -> Self::Future {
        // println!("upgrade inbound called");
        future::ready(Ok(i))
    }
}

impl<C> OutboundUpgrade<C> for FakeProtocolConfig {
    type Output = Negotiated<C>;
    type Error = Void;
    type Future = Ready<Result<Negotiated<C>, Self::Error>>;

    fn upgrade_outbound(self, i: Negotiated<C>, _: Self::Info) -> Self::Future {
        // println!("upgrade outbound called");
        future::ready(Ok(i))
    }
}

impl<TSubstream> libp2p::swarm::NetworkBehaviour for FakeConfig<TSubstream>
where
    TSubstream: AsyncRead + AsyncWrite + Unpin,
{
    /// Handler for all the protocols the network behaviour supports.
    type ProtocolsHandler = FakeHandler<TSubstream>;

    /// Event generated by the `NetworkBehaviour` and that the swarm will report back.
    type OutEvent = FakeEvent;

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
        // println!("==== creating new handler");
        FakeHandler {
            substreams: vec![],
            marker: PhantomData,
        }
    }

    /// Addresses that this behaviour is aware of for this specific peer, and that may allow
    /// reaching the peer.
    ///
    /// The addresses will be tried in the order returned by this function, which means that they
    /// should be ordered by decreasing likelihood of reachability. In other words, the first
    /// address should be the most likely to be reachable.
    fn addresses_of_peer(&mut self, peer_id: &PeerId) -> Vec<Multiaddr> {
        vec![]
    }

    /// Indicates the behaviour that we connected to the node with the given peer id through the
    /// given endpoint.
    ///
    /// This node now has a handler (as spawned by `new_handler`) running in the background.
    fn inject_connected(&mut self, peer_id: PeerId, endpoint: ConnectedPoint) {
        // println!("FakeConfig: inject connected {:?}", peer_id);
    }

    /// Indicates the behaviour that we disconnected from the node with the given peer id. The
    /// endpoint is the one we used to be connected to.
    ///
    /// There is no handler running anymore for this node. Any event that has been sent to it may
    /// or may not have been processed by the handler.
    fn inject_disconnected(&mut self, peer_id: &PeerId, endpoint: ConnectedPoint) {}

    /// Informs the behaviour about an event generated by the handler dedicated to the peer identified by `peer_id`.
    /// for the behaviour.
    ///
    /// The `peer_id` is guaranteed to be in a connected state. In other words, `inject_connected`
    /// has previously been called with this `PeerId`.
    fn inject_node_event(
        &mut self,
        peer_id: PeerId,
        event: <<Self::ProtocolsHandler as IntoProtocolsHandler>::Handler as ProtocolsHandler>::OutEvent,
    ) {
    }

    /// Indicates to the behaviour that we tried to reach an address, but failed.
    ///
    /// If we were trying to reach a specific node, its ID is passed as parameter. If this is the
    /// last address to attempt for the given node, then `inject_dial_failure` is called afterwards.
    fn inject_addr_reach_failure(
        &mut self,
        _peer_id: Option<&PeerId>,
        _addr: &Multiaddr,
        _error: &dyn Error,
    ) {
    }

    /// Indicates to the behaviour that we tried to dial all the addresses known for a node, but
    /// failed.
    ///
    /// The `peer_id` is guaranteed to be in a disconnected state. In other words,
    /// `inject_connected` has not been called, or `inject_disconnected` has been called since then.
    fn inject_dial_failure(&mut self, _peer_id: &PeerId) {}

    /// Indicates to the behaviour that we have started listening on a new multiaddr.
    fn inject_new_listen_addr(&mut self, _addr: &Multiaddr) {}

    /// Indicates to the behaviour that a new multiaddr we were listening on has expired,
    /// which means that we are no longer listening in it.
    fn inject_expired_listen_addr(&mut self, _addr: &Multiaddr) {}

    /// Indicates to the behaviour that we have discovered a new external address for us.
    fn inject_new_external_addr(&mut self, _addr: &Multiaddr) {}

    /// A listener experienced an error.
    fn inject_listener_error(&mut self, _id: ListenerId, _err: &(dyn std::error::Error + 'static)) {
    }

    /// A listener closed.
    fn inject_listener_closed(&mut self, _id: ListenerId) {}

    /// Polls for things that swarm should do.
    ///
    /// This API mimics the API of the `Stream` trait. The method may register the current task in
    /// order to wake it up at a later point in time.
    fn poll(&mut self, cx: &mut Context, params: &mut impl PollParameters)
        -> Poll<NetworkBehaviourAction<<<Self::ProtocolsHandler as IntoProtocolsHandler>::Handler as ProtocolsHandler>::InEvent, Self::OutEvent>>
    {
        // println!("poll called");
        Poll::Pending
    }
}

pub struct FakeHandler<TSubstream> {
    substreams: Vec<Negotiated<TSubstream>>,
    /// Marker to pin the generic.
    marker: PhantomData<TSubstream>,
}

impl<TSubstream> ProtocolsHandler for FakeHandler<TSubstream>
where
    TSubstream: AsyncRead + AsyncWrite + Unpin,
{
    type InEvent = Void;
    type OutEvent = Void;
    type Error = Void;
    type Substream = TSubstream;
    type InboundProtocol = FakeProtocolConfig;
    type OutboundProtocol = FakeProtocolConfig;
    type OutboundOpenInfo = Void;

    #[inline]
    fn listen_protocol(&self) -> SubstreamProtocol<Self::InboundProtocol> {
        // println!("fake handler: listen protocol");
        SubstreamProtocol::new(FakeProtocolConfig {})
    }

    #[inline]
    fn inject_fully_negotiated_inbound(
        &mut self,
        protocol: <Self::InboundProtocol as InboundUpgrade<TSubstream>>::Output,
    ) {
        // println!("fake handler: inject fully negotiated inbound");
        self.substreams.push(protocol)
    }

    #[inline]
    fn inject_fully_negotiated_outbound(
        &mut self,
        protocol: <Self::OutboundProtocol as OutboundUpgrade<TSubstream>>::Output,
        _: Self::OutboundOpenInfo,
    ) {
        // println!("fake handler: inject fully negotiated outbound");
        self.substreams.push(protocol)
    }

    #[inline]
    fn inject_event(&mut self, _: Self::InEvent) {
        // println!("fake handler: inject event");
    }

    #[inline]
    fn inject_dial_upgrade_error(
        &mut self,
        _: Self::OutboundOpenInfo,
        _: ProtocolsHandlerUpgrErr<
            <Self::OutboundProtocol as OutboundUpgrade<Self::Substream>>::Error,
        >,
    ) {
        // println!("fake handler: inject dial upgrade error");
    }

    #[inline]
    fn connection_keep_alive(&self) -> KeepAlive {
        // println!("fake handler: connection keep alive");
        KeepAlive::Yes
    }

    #[inline]
    fn poll(
        &mut self,
        _: &mut Context,
    ) -> Poll<
        ProtocolsHandlerEvent<
            Self::OutboundProtocol,
            Self::OutboundOpenInfo,
            Self::OutEvent,
            Self::Error,
        >,
    > {
        // println!("fake handler: poll called");
        Poll::Pending
    }
}

pub enum FakeEvent {}

pub struct FakeProtocolConfig {}
