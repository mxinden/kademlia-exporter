use crate::{cloud_provider_db, config::DhtConfig};
use client::Client;
use futures::prelude::*;
use futures_timer::Delay;
use libp2p::{
    identify::IdentifyEvent,
    kad::{GetClosestPeersOk, KademliaEvent, QueryResult},
    multiaddr::{Multiaddr, Protocol},
    ping::{PingEvent, PingSuccess},
    PeerId,
};
use log::info;
use maxminddb::{geoip2, Reader};
use node_store::{Node, NodeStore};
use open_metrics_client::counter::Counter;
use open_metrics_client::encoding::text::Encode;
use open_metrics_client::family::Family;
use open_metrics_client::gauge::Gauge;
use open_metrics_client::histogram::{exponential_series, Histogram};
use open_metrics_client::registry::{Descriptor, DynSendRegistry};
use std::{
    collections::HashMap,
    convert::TryInto,
    error::Error,
    io::Write,
    net::IpAddr,
    pin::Pin,
    sync::atomic::AtomicU64,
    task::{Context, Poll},
    time::{Duration, Instant},
};

mod client;
mod node_store;

const TICK_INTERVAL: Duration = Duration::from_secs(1);

pub(crate) struct Exporter {
    // TODO: Introduce dht id new type.
    clients: HashMap<String, Client>,
    node_stores: HashMap<String, NodeStore>,
    ip_db: Option<Reader<Vec<u8>>>,
    cloud_provider_db: Option<cloud_provider_db::Db>,
    /// Set of in-flight random peer id lookups.
    ///
    /// When a lookup returns the entry is dropped and thus the duratation is
    /// observed through `<HistogramTimer as Drop>::drop`.
    in_flight_lookups: HashMap<PeerId, Instant>,
    tick: Delay,
    metrics: Metrics,
    /// An exporter periodically reconnects to each discovered node to probe
    /// whether it is still online.
    nodes_to_probe_periodically: HashMap<String, Vec<PeerId>>,
}

impl Exporter {
    pub(crate) fn new(
        dhts: Vec<DhtConfig>,
        ip_db: Option<Reader<Vec<u8>>>,
        cloud_provider_db: Option<cloud_provider_db::Db>,
        registry: &mut DynSendRegistry,
    ) -> Result<Self, Box<dyn Error>> {
        let metrics = Metrics::register(registry);

        let clients = dhts
            .clone()
            .into_iter()
            .map(|config| (config.name.clone(), client::Client::new(config).unwrap()))
            .collect();

        let node_store_metrics = node_store::Metrics::register(registry);
        let node_stores = dhts
            .clone()
            .into_iter()
            .map(|DhtConfig { name, .. }| {
                (
                    name.clone(),
                    NodeStore::new(name, node_store_metrics.clone()),
                )
            })
            .collect();

        let nodes_to_probe_periodically = dhts
            .into_iter()
            .map(|DhtConfig { name, .. }| (name, vec![]))
            .collect();

        Ok(Exporter {
            clients,
            metrics,
            ip_db,
            cloud_provider_db,
            node_stores,

            tick: futures_timer::Delay::new(TICK_INTERVAL),

            in_flight_lookups: HashMap::new(),
            nodes_to_probe_periodically,
        })
    }

    fn record_event(&mut self, name: String, event: client::Event) {
        match event {
            client::Event::Ping(PingEvent { peer, result }) => {
                // Update node store.
                match result {
                    Ok(_) => self
                        .node_stores
                        .get_mut(&name)
                        .unwrap()
                        .observed_node(Node::new(peer.clone())),
                    Err(_) => self
                        .node_stores
                        .get_mut(&name)
                        .unwrap()
                        .observed_down(&peer),
                }

                let country = self
                    .node_stores
                    .get_mut(&name)
                    .unwrap()
                    .get_peer(&peer)
                    .map(|p| p.country.clone())
                    .flatten()
                    .unwrap_or_else(|| "unknown".to_string());

                let event = match result {
                    // Sent a ping and received back a pong.
                    Ok(PingSuccess::Ping { rtt }) => {
                        self.metrics
                            .ping_duration
                            .get_or_create(&vec![
                                ("name".to_string(), name.clone()),
                                ("country".to_string(), country.clone()),
                            ])
                            .observe(rtt.as_secs_f64());
                        "received_pong"
                    }
                    // Received a ping and sent back a pong.
                    Ok(PingSuccess::Pong) => "received_ping",
                    Err(_) => "error",
                };

                self.metrics
                    .event_counter
                    .get_or_create(&EventCounterLabels {
                        dht: name.clone(),
                        behaviour: BehaviourLabel::Ping,
                        event: event.to_string(),
                    })
                    .inc();
            }
            client::Event::Identify(event) => match *event {
                IdentifyEvent::Error { .. } => {
                    self.metrics
                        .event_counter
                        .get_or_create(&EventCounterLabels {
                            dht: name.clone(),
                            behaviour: BehaviourLabel::Identify,
                            event: "error".to_string(),
                        })
                        .inc();
                }
                IdentifyEvent::Sent { .. } => {
                    self.metrics
                        .event_counter
                        .get_or_create(&EventCounterLabels {
                            dht: name.clone(),
                            behaviour: BehaviourLabel::Identify,
                            event: "sent".to_string(),
                        })
                        .inc();
                }
                IdentifyEvent::Received { peer_id, .. } => {
                    self.node_stores
                        .get_mut(&name)
                        .unwrap()
                        .observed_node(Node::new(peer_id));

                    self.metrics
                        .event_counter
                        .get_or_create(&EventCounterLabels {
                            dht: name.clone(),
                            behaviour: BehaviourLabel::Identify,
                            event: "received".to_string(),
                        })
                        .inc();
                }
            },
            client::Event::Kademlia(event) => self.record_kademlia_event(name, *event),
        }
    }

    fn record_kademlia_event(&mut self, name: String, event: KademliaEvent) {
        match event {
            KademliaEvent::QueryResult { result, stats, .. } => {
                let query_name;

                match result {
                    QueryResult::Bootstrap(_) => {
                        query_name = "bootstrap";
                        self.metrics
                            .event_counter
                            .get_or_create(&EventCounterLabels {
                                dht: name.clone(),
                                behaviour: BehaviourLabel::Kad,
                                event: query_name.to_string(),
                            })
                            .inc();
                    }
                    QueryResult::GetClosestPeers(res) => {
                        query_name = "get_closest_peers";
                        self.metrics
                            .event_counter
                            .get_or_create(&EventCounterLabels {
                                dht: name.clone(),
                                behaviour: BehaviourLabel::Kad,
                                event: query_name.to_string(),
                            })
                            .inc();

                        // Record lookup latency.
                        let result_label = if res.is_ok() { "ok" } else { "error" };
                        let peer_id = match res {
                            Ok(GetClosestPeersOk { key, .. }) => PeerId::from_bytes(&key),
                            Err(err) => PeerId::from_bytes(&err.into_key()),
                        }
                        .unwrap();
                        let duration =
                            Instant::now() - self.in_flight_lookups.remove(&peer_id).unwrap();
                        self.metrics
                            .kad_random_node_lookup_duration
                            .get_or_create(&vec![
                                ("dht".to_string(), name.to_string()),
                                ("result".to_string(), result_label.to_string()),
                            ])
                            .observe(duration.as_secs_f64());
                    }
                    QueryResult::GetProviders(_) => {
                        query_name = "get_providers";
                        self.metrics
                            .event_counter
                            .get_or_create(&EventCounterLabels {
                                dht: name.clone(),
                                behaviour: BehaviourLabel::Kad,
                                event: query_name.to_string(),
                            })
                            .inc();
                    }
                    QueryResult::StartProviding(_) => {
                        query_name = "start_providing";
                        self.metrics
                            .event_counter
                            .get_or_create(&EventCounterLabels {
                                dht: name.clone(),
                                behaviour: BehaviourLabel::Kad,
                                event: query_name.to_string(),
                            })
                            .inc();
                    }
                    QueryResult::RepublishProvider(_) => {
                        query_name = "republish_provider";
                        self.metrics
                            .event_counter
                            .get_or_create(&EventCounterLabels {
                                dht: name.clone(),
                                behaviour: BehaviourLabel::Kad,
                                event: query_name.to_string(),
                            })
                            .inc();
                    }
                    QueryResult::GetRecord(_) => {
                        query_name = "get_record";
                        self.metrics
                            .event_counter
                            .get_or_create(&EventCounterLabels {
                                dht: name.clone(),
                                behaviour: BehaviourLabel::Kad,
                                event: query_name.to_string(),
                            })
                            .inc();
                    }
                    QueryResult::PutRecord(_) => {
                        query_name = "put_record";
                        self.metrics
                            .event_counter
                            .get_or_create(&EventCounterLabels {
                                dht: name.clone(),
                                behaviour: BehaviourLabel::Kad,
                                event: query_name.to_string(),
                            })
                            .inc();
                    }
                    QueryResult::RepublishRecord(_) => {
                        query_name = "republish_record";
                        self.metrics
                            .event_counter
                            .get_or_create(&EventCounterLabels {
                                dht: name.clone(),
                                behaviour: BehaviourLabel::Kad,
                                event: query_name.to_string(),
                            })
                            .inc();
                    }
                }

                self.metrics
                    .kad_query_stats
                    .get_or_create(&vec![
                        ("dht".to_string(), name.clone()),
                        ("query".to_string(), query_name.to_string()),
                        ("stat".to_string(), "num_requests".to_string()),
                    ])
                    .observe(stats.num_requests().into());
                self.metrics
                    .kad_query_stats
                    .get_or_create(&vec![
                        ("dht".to_string(), name.clone()),
                        ("query".to_string(), query_name.to_string()),
                        ("stat".to_string(), "num_successes".to_string()),
                    ])
                    .observe(stats.num_successes().into());
                self.metrics
                    .kad_query_stats
                    .get_or_create(&vec![
                        ("dht".to_string(), name.clone()),
                        ("query".to_string(), query_name.to_string()),
                        ("stat".to_string(), "num_failures".to_string()),
                    ])
                    .observe(stats.num_failures().into());
                self.metrics
                    .kad_query_stats
                    .get_or_create(&vec![
                        ("dht".to_string(), name.clone()),
                        ("query".to_string(), query_name.to_string()),
                        ("stat".to_string(), "num_pending".to_string()),
                    ])
                    .observe(stats.num_pending().into());
                if let Some(duration) = stats.duration() {
                    self.metrics
                        .kad_query_stats
                        .get_or_create(&vec![
                            ("dht".to_string(), name.clone()),
                            ("query".to_string(), query_name.to_string()),
                            ("stat".to_string(), "duration".to_string()),
                        ])
                        .observe(duration.as_secs_f64());
                }
            }
            KademliaEvent::RoutablePeer { peer, address } => {
                self.metrics
                    .event_counter
                    .get_or_create(&EventCounterLabels {
                        dht: name.clone(),
                        behaviour: BehaviourLabel::Kad,
                        event: "routable_peer".to_string(),
                    })
                    .inc();

                self.observe_with_address(name, peer, vec![address]);
            }
            KademliaEvent::PendingRoutablePeer { peer, address } => {
                self.metrics
                    .event_counter
                    .get_or_create(&EventCounterLabels {
                        dht: name.clone(),
                        behaviour: BehaviourLabel::Kad,
                        event: "pending_routable_peer".to_string(),
                    })
                    .inc();

                self.observe_with_address(name, peer, vec![address]);
            }
            KademliaEvent::RoutingUpdated {
                peer, addresses, ..
            } => {
                self.metrics
                    .event_counter
                    .get_or_create(&EventCounterLabels {
                        dht: name.clone(),
                        behaviour: BehaviourLabel::Kad,
                        event: "routing_updated".to_string(),
                    })
                    .inc();

                self.observe_with_address(name, peer, addresses.into_vec());
            }
            KademliaEvent::UnroutablePeer { .. } => {
                self.metrics
                    .event_counter
                    .get_or_create(&EventCounterLabels {
                        dht: name.clone(),
                        behaviour: BehaviourLabel::Kad,
                        event: "unroutable_peer".to_string(),
                    })
                    .inc();
            }
        }
    }

    fn observe_with_address(&mut self, name: String, peer: PeerId, addresses: Vec<Multiaddr>) {
        let mut node = Node::new(peer);
        if let Some(country) = self.multiaddresses_to_country_code(addresses.iter()) {
            node = node.with_country(country);
        }
        if let Some(provider) = self.multiaddresses_to_cloud_provider(addresses.iter()) {
            node = node.with_cloud_provider(provider);
        }
        self.node_stores.get_mut(&name).unwrap().observed_node(node);
    }

    fn multiaddresses_to_cloud_provider<'a>(
        &self,
        addresses: impl Iterator<Item = &'a Multiaddr>,
    ) -> Option<String> {
        for address in addresses {
            let provider = self.multiaddress_to_cloud_provider(address);
            if provider.is_some() {
                return provider;
            }
        }

        None
    }

    fn multiaddress_to_cloud_provider(&self, address: &Multiaddr) -> Option<String> {
        let ip_address = match address.iter().next()? {
            Protocol::Ip4(addr) => Some(addr),
            _ => None,
        }?;

        if let Some(db) = &self.cloud_provider_db {
            return db.get_provider(ip_address);
        }

        None
    }

    fn multiaddresses_to_country_code<'a>(
        &self,
        addresses: impl Iterator<Item = &'a Multiaddr>,
    ) -> Option<String> {
        for address in addresses {
            let country = self.multiaddress_to_country_code(address);
            if country.is_some() {
                return country;
            }
        }

        None
    }

    fn multiaddress_to_country_code(&self, address: &Multiaddr) -> Option<String> {
        let ip_address = match address.iter().next()? {
            Protocol::Ip4(addr) => Some(IpAddr::V4(addr)),
            Protocol::Ip6(addr) => Some(IpAddr::V6(addr)),
            _ => None,
        }?;

        if let Some(ip_db) = &self.ip_db {
            return Some(
                ip_db
                    .lookup::<geoip2::City>(ip_address)
                    .ok()?
                    .country?
                    .iso_code?
                    .to_string(),
            );
        }

        None
    }
}

impl Future for Exporter {
    type Output = ();
    fn poll(mut self: Pin<&mut Self>, ctx: &mut Context) -> Poll<Self::Output> {
        let this = &mut *self;

        if let Poll::Ready(()) = this.tick.poll_unpin(ctx) {
            this.tick = Delay::new(TICK_INTERVAL);

            for node_store in &mut this.node_stores.values_mut() {
                node_store.tick();
            }

            for (dht, nodes) in &mut this.nodes_to_probe_periodically {
                match nodes.pop() {
                    Some(peer_id) => {
                        info!("Checking if {:?} is still online.", &peer_id);
                        match this.clients.get_mut(dht).unwrap().dial(&peer_id) {
                            // New connection was established.
                            Ok(true) => {
                                this.metrics
                                    .meta_node_still_online_check_triggered
                                    .get_or_create(&vec![("dht".to_string(), dht.clone())])
                                    .inc();
                            }
                            // Already connected to node.
                            Ok(false) => {}
                            // Connection limit reached. Retry later.
                            Err(_) => nodes.insert(0, peer_id),
                        }
                    }
                    // List is empty. Reconnected to every peer. Refill the
                    // list.
                    None => {
                        nodes.append(
                            &mut this
                                .node_stores
                                .get(dht)
                                .unwrap()
                                .iter()
                                .map(|n| n.peer_id.clone())
                                .collect(),
                        );
                    }
                }
            }

            // Trigger a random lookup for each client.
            for (name, client) in this.clients.iter_mut() {
                this.metrics
                    .meta_random_node_lookup_triggered
                    .get_or_create(&vec![("dht".to_string(), name.clone())])
                    .inc();
                let random_peer = PeerId::random();
                client.get_closest_peers(random_peer.clone());
                this.in_flight_lookups.insert(random_peer, Instant::now());
            }

            for (name, client) in this.clients.iter() {
                let info = client.network_info();
                this.metrics
                    .meta_libp2p_network_info_num_peers
                    .get_or_create(&vec![("dht".to_string(), name.clone())])
                    .set(info.num_peers().try_into().unwrap());
                this.metrics
                    .meta_libp2p_network_info_num_connections
                    .get_or_create(&vec![("dht".to_string(), name.clone())])
                    .set(info.connection_counters().num_connections().into());
                this.metrics
                    .meta_libp2p_network_info_num_connections_established
                    .get_or_create(&vec![("dht".to_string(), name.clone())])
                    .set(info.connection_counters().num_established().into());
                this.metrics
                    .meta_libp2p_network_info_num_connections_pending
                    .get_or_create(&vec![("dht".to_string(), name.clone())])
                    .set(info.connection_counters().num_pending().into());
            }
        }

        let mut events = vec![];

        for (name, client) in &mut this.clients {
            loop {
                match client.poll_next_unpin(ctx) {
                    Poll::Ready(Some(event)) => events.push((name.clone(), event)),
                    Poll::Ready(None) => return Poll::Ready(()),
                    Poll::Pending => break,
                }
            }
        }

        for (name, event) in events {
            this.record_event(name, event);
        }

        Poll::Pending
    }
}

#[derive(Clone, Hash, PartialEq, Eq)]
struct EventCounterLabels {
    // TODO: Could one not use Cow here as EventCounterLabels would be borrowed
    // in most cases anyways?
    dht: String,
    behaviour: BehaviourLabel,
    event: String,
}

#[derive(Clone, Hash, PartialEq, Eq)]
enum BehaviourLabel {
    Identify,
    Ping,
    Kad,
}

impl Encode for EventCounterLabels {
    fn encode(&self, writer: &mut dyn Write) -> Result<(), std::io::Error> {
        writer.write(b"dht")?;
        writer.write(b"=\"")?;
        writer.write(self.dht.as_bytes())?;
        writer.write(b"\"")?;
        writer.write(b",")?;

        writer.write(b"behaviour")?;
        writer.write(b"=\"")?;
        let behaviour = match self.behaviour {
            BehaviourLabel::Identify => "identify",
            BehaviourLabel::Ping => "ping",
            BehaviourLabel::Kad => "kad",
        };
        writer.write(behaviour.as_bytes())?;
        writer.write(b"\"")?;
        writer.write(b",")?;

        writer.write(b"event")?;
        writer.write(b"=\"")?;
        writer.write(self.event.as_bytes())?;
        writer.write(b"\"")?;

        Ok(())
    }
}

struct Metrics {
    event_counter: Family<EventCounterLabels, Counter<AtomicU64>>,

    ping_duration: Family<Vec<(String, String)>, Histogram>,
    kad_random_node_lookup_duration: Family<Vec<(String, String)>, Histogram>,
    kad_query_stats: Family<Vec<(String, String)>, Histogram>,

    meta_random_node_lookup_triggered: Family<Vec<(String, String)>, Counter<AtomicU64>>,
    meta_node_still_online_check_triggered: Family<Vec<(String, String)>, Counter<AtomicU64>>,
    meta_libp2p_network_info_num_peers: Family<Vec<(String, String)>, Gauge<AtomicU64>>,
    meta_libp2p_network_info_num_connections: Family<Vec<(String, String)>, Gauge<AtomicU64>>,
    meta_libp2p_network_info_num_connections_pending:
        Family<Vec<(String, String)>, Gauge<AtomicU64>>,
    meta_libp2p_network_info_num_connections_established:
        Family<Vec<(String, String)>, Gauge<AtomicU64>>,
}

impl Metrics {
    fn register(registry: &mut DynSendRegistry) -> Metrics {
        let event_counter = Family::new();
        registry.register(
            Descriptor::new(
                "counter",
                "Libp2p network behaviour events.",
                "network_behaviour_event",
            ),
            Box::new(event_counter.clone()),
        );

        let kad_random_node_lookup_duration =
            Family::new_with_constructor(|| Histogram::new(exponential_series(0.1, 2.0, 10)));
        registry.register(
            Descriptor::new(
                "histogram",
                "Duration of random Kademlia node lookup.",
                "kad_random_node_lookup_duration",
            ),
            Box::new(kad_random_node_lookup_duration.clone()),
        );
        // &["dht", "result"],

        let kad_query_stats =
            Family::new_with_constructor(|| Histogram::new(exponential_series(1.0, 2.0, 10)));
        registry.register(
            Descriptor::new(
                "histogram",
                "Kademlia query statistics (number of requests, successes, failures and duration).",
                "kad_query_stats",
            ),
            Box::new(kad_query_stats.clone()),
        );
        // &["dht", "query", "stat"],

        let ping_duration = Family::new_with_constructor(|| {
            Histogram::new(
                vec![
                    0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
                ]
                .into_iter(),
            )
        });
        registry.register(
            Descriptor::new(
                "histogram",
                "Duration of a ping round trip.",
                "ping_duration",
            ),
            Box::new(ping_duration.clone()),
        );
        // &["dht", "country"],

        let meta_random_node_lookup_triggered = Family::new();
        registry.register(
            Descriptor::new(
                "counter",
                "Number of times a random Kademlia node lookup was triggered.",
                "meta_random_node_lookup_triggered",
            ),
            Box::new(meta_random_node_lookup_triggered.clone()),
        );
        // &["dht"],

        let meta_node_still_online_check_triggered = Family::new();
        registry.register(Descriptor::new(
            "counter",
            "Number of times a connection to a node was established to ensure it is still online.",
            "meta_node_still_online_check_triggered",
        ), Box::new(meta_node_still_online_check_triggered.clone()));

        let meta_libp2p_network_info_num_peers = Family::new();
        registry.register(
            Descriptor::new(
                "gauge",
                "The total number of connected peers.",
                "meta_libp2p_network_info_num_peers",
            ),
            Box::new(meta_libp2p_network_info_num_peers.clone()),
        );
        // &["dht"],

        let meta_libp2p_network_info_num_connections = Family::new();
        registry.register(
            Descriptor::new(
                "gauge",
                "The total number of connections, both established and pending.",
                "meta_libp2p_network_info_num_connections",
            ),
            Box::new(meta_libp2p_network_info_num_peers.clone()),
        );
        // &["dht"],

        let meta_libp2p_network_info_num_connections_pending = Family::new();
        registry.register(
            Descriptor::new(
                "gauge",
                "The total number of pending connections, both incoming and outgoing.",
                "meta_libp2p_network_info_num_connections_pending",
            ),
            Box::new(meta_libp2p_network_info_num_connections_pending.clone()),
        );
        // &["dht"],

        let meta_libp2p_network_info_num_connections_established = Family::new();
        registry.register(
            Descriptor::new(
                "gauge",
                "The total number of established connections.",
                "meta_libp2p_network_info_num_connections_established",
            ),
            Box::new(meta_libp2p_network_info_num_connections_established.clone()),
        );
        // &["dht"],

        Metrics {
            event_counter,

            ping_duration,
            kad_random_node_lookup_duration,
            kad_query_stats,

            meta_random_node_lookup_triggered,
            meta_node_still_online_check_triggered,
            meta_libp2p_network_info_num_peers,
            meta_libp2p_network_info_num_connections,
            meta_libp2p_network_info_num_connections_pending,
            meta_libp2p_network_info_num_connections_established,
        }
    }
}
