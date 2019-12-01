// TODO: Continue implementing fake event

use async_std::{io, task};
use futures::future::{self};
use futures::prelude::*;
use libp2p::{
    core::{
        self, muxing::StreamMuxerBox, nodes::ListenerId, transport::boxed::Boxed,
        transport::Transport, upgrade::Negotiated, ConnectedPoint, InboundUpgrade, Multiaddr,
        OutboundUpgrade, UpgradeInfo,
    },
    dns,
    identify::{Identify, IdentifyEvent},
    identity::Keypair,
    kad::{
        record::{store::MemoryStore, Key},
        Kademlia, KademliaEvent, PutRecordOk, Quorum, Record,
    },
    mdns::{Mdns, MdnsEvent},
    noise,
    ping::{Ping, PingConfig, PingEvent},
    swarm::NetworkBehaviourEventProcess,
    tcp, yamux, NetworkBehaviour, PeerId, Swarm,
};
use std::{
    convert::TryInto,
    error::Error,
    str::FromStr,
    task::{Context, Poll},
    time::Duration,
};


mod fake_substrate_protocol;

use fake_substrate_protocol::{FakeSubstrateConfig, FakeSubstrateEvent};

fn main() -> Result<(), Box<dyn Error>> {
    let bootnode: Multiaddr = "/dns4/p2p.cc3-5.kusama.network/tcp/30100"
        .try_into()
        .unwrap();
    let bootnode_peer_id =
        PeerId::from_str("QmdePe9MiAJT4yHT2tEwmazCsckAZb19uaoSUgRDffPq3G").unwrap();
    env_logger::init();

    // Create a random key for ourselves.
    let local_key = Keypair::generate_ed25519();
    let local_peer_id = PeerId::from(local_key.public());

    // Set up a an encrypted DNS-enabled TCP Transport over the Mplex protocol.
    let transport = build_transport(local_key.clone());

    // We create a custom network behaviour that combines Kademlia and mDNS.
    #[derive(NetworkBehaviour)]
    struct MyBehaviour<TSubstream: AsyncRead + AsyncWrite> {
        kademlia: Kademlia<TSubstream, MemoryStore>,
        mdns: Mdns<TSubstream>,
        fake: FakeSubstrateConfig<TSubstream>,
        ping: Ping<TSubstream>,
        identify: Identify<TSubstream>,
    }

    impl<T> NetworkBehaviourEventProcess<MdnsEvent> for MyBehaviour<T>
    where
        T: AsyncRead + AsyncWrite,
    {
        // Called when `mdns` produces an event.
        fn inject_event(&mut self, event: MdnsEvent) {
            if let MdnsEvent::Discovered(list) = event {
                for (peer_id, multiaddr) in list {
                    self.kademlia.add_address(&peer_id, multiaddr);
                }
            }
        }
    }

    impl<T> NetworkBehaviourEventProcess<FakeSubstrateEvent> for MyBehaviour<T>
    where
        T: AsyncRead + AsyncWrite,
    {
        fn inject_event(&mut self, _event: FakeSubstrateEvent) {
            println!("GOT FAKE EVENT");
        }
    }

    impl<T> NetworkBehaviourEventProcess<PingEvent> for MyBehaviour<T>
    where
        T: AsyncRead + AsyncWrite,
    {
        fn inject_event(&mut self, _event: PingEvent) {
            println!("Got PingEvent");
        }
    }

    impl<T> NetworkBehaviourEventProcess<IdentifyEvent> for MyBehaviour<T>
    where
        T: AsyncRead + AsyncWrite,
    {
        fn inject_event(&mut self, _event: IdentifyEvent) {
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
                _e => {} // println!("Kademlia Event: {:?}", e)
            }
        }
    }

    // Create a swarm to manage peers and events.
    let mut swarm = {
        // Create a Kademlia behaviour.
        let store = MemoryStore::new(local_peer_id.clone());
        let kademlia = Kademlia::new(local_peer_id.clone(), store);
        let mdns = task::block_on(Mdns::new())?;
        let fake = FakeSubstrateConfig::new();
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
    Swarm::listen_on(&mut swarm, "/ip4/0.0.0.0/tcp/0".parse()?)?;

    swarm
        .kademlia
        .add_address(&bootnode_peer_id, bootnode.clone());

    swarm.kademlia.bootstrap();

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
