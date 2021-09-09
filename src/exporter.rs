use crate::{cloud_provider_db, config::DhtConfig};
use client::Client;
use futures::prelude::*;
use futures_timer::Delay;
use libp2p::{
    identify::{IdentifyEvent, IdentifyInfo},
    kad::KademliaEvent,
    multiaddr::{Multiaddr, Protocol},
    ping::PingEvent,
    PeerId,
};
use log::info;
use maxminddb::{geoip2, Reader};
use node_store::{Node, NodeStore};
use open_metrics_client::metrics::counter::Counter;
use open_metrics_client::metrics::family::Family;
use open_metrics_client::metrics::gauge::Gauge;
use open_metrics_client::registry::Registry;
use std::{
    collections::HashMap,
    convert::TryInto,
    error::Error,
    net::IpAddr,
    pin::Pin,
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
        registry: &mut Registry,
    ) -> Result<Self, Box<dyn Error>> {
        let clients = dhts
            .clone()
            .into_iter()
            .map(|config| {
                let sub_registry =
                    registry.sub_registry_with_label(("dht".into(), config.name.clone().into()));
                (
                    config.name.clone(),
                    client::Client::new(config, sub_registry).unwrap(),
                )
            })
            .collect();

        let sub_registry = registry.sub_registry_with_prefix("kademlia_exporter");

        let metrics = Metrics::register(sub_registry);

        let node_store_metrics = node_store::Metrics::register(sub_registry);
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
            }
            client::Event::Identify(event) => match *event {
                IdentifyEvent::Error { .. } => {}
                IdentifyEvent::Sent { .. } => {}
                IdentifyEvent::Received {
                    peer_id,
                    info: IdentifyInfo { listen_addrs, .. },
                } => {
                    self.observe_with_address(name, peer_id, listen_addrs);
                }
                IdentifyEvent::Pushed { .. } => {
                    unreachable!("Exporter never pushes identify information.")
                }
            },
            client::Event::Kademlia(event) => match *event {
                KademliaEvent::RoutablePeer { peer, address } => {
                    self.observe_with_address(name, peer, vec![address]);
                }
                KademliaEvent::PendingRoutablePeer { peer, address } => {
                    self.observe_with_address(name, peer, vec![address]);
                }
                KademliaEvent::RoutingUpdated {
                    peer, addresses, ..
                } => {
                    self.observe_with_address(name, peer, addresses.into_vec());
                }
                _ => {}
            },
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
                this.metrics
                    .meta_libp2p_bandwidth_inbound
                    .get_or_create(&vec![("dht".to_string(), name.clone())])
                    .set(client.total_inbound());
                this.metrics
                    .meta_libp2p_bandwidth_outbound
                    .get_or_create(&vec![("dht".to_string(), name.clone())])
                    .set(client.total_outbound());
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

struct Metrics {
    meta_random_node_lookup_triggered: Family<Vec<(String, String)>, Counter>,
    meta_node_still_online_check_triggered: Family<Vec<(String, String)>, Counter>,
    meta_libp2p_network_info_num_peers: Family<Vec<(String, String)>, Gauge>,
    meta_libp2p_network_info_num_connections: Family<Vec<(String, String)>, Gauge>,
    meta_libp2p_network_info_num_connections_pending: Family<Vec<(String, String)>, Gauge>,
    meta_libp2p_network_info_num_connections_established: Family<Vec<(String, String)>, Gauge>,
    meta_libp2p_bandwidth_inbound: Family<Vec<(String, String)>, Gauge>,
    meta_libp2p_bandwidth_outbound: Family<Vec<(String, String)>, Gauge>,
}

impl Metrics {
    fn register(registry: &mut Registry) -> Metrics {
        let meta_random_node_lookup_triggered = Family::default();
        registry.register(
            "meta_random_node_lookup_triggered",
            "Number of times a random Kademlia node lookup was triggered",
            Box::new(meta_random_node_lookup_triggered.clone()),
        );

        let meta_node_still_online_check_triggered = Family::default();
        registry.register(
            "meta_node_still_online_check_triggered",
            "Number of times a connection to a node was established to ensure it is still online",
            Box::new(meta_node_still_online_check_triggered.clone()),
        );

        let meta_libp2p_network_info_num_peers = Family::default();
        registry.register(
            "meta_libp2p_network_info_num_peers",
            "The total number of connected peers",
            Box::new(meta_libp2p_network_info_num_peers.clone()),
        );

        let meta_libp2p_network_info_num_connections = Family::default();
        registry.register(
            "meta_libp2p_network_info_num_connections",
            "The total number of connections, both established and pending",
            Box::new(meta_libp2p_network_info_num_peers.clone()),
        );

        let meta_libp2p_network_info_num_connections_pending = Family::default();
        registry.register(
            "meta_libp2p_network_info_num_connections_pending",
            "The total number of pending connections, both incoming and outgoing",
            Box::new(meta_libp2p_network_info_num_connections_pending.clone()),
        );

        let meta_libp2p_network_info_num_connections_established = Family::default();
        registry.register(
            "meta_libp2p_network_info_num_connections_established",
            "The total number of established connections",
            Box::new(meta_libp2p_network_info_num_connections_established.clone()),
        );

        let meta_libp2p_bandwidth_inbound = Family::default();
        registry.register(
            "meta_libp2p_bandwidth_inbound",
            "The total number of bytes received on the socket",
            Box::new(meta_libp2p_bandwidth_inbound.clone()),
        );

        let meta_libp2p_bandwidth_outbound = Family::default();
        registry.register(
            "meta_libp2p_bandwidth_outbound",
            "The total number of bytes sent on the socket",
            Box::new(meta_libp2p_bandwidth_outbound.clone()),
        );

        Metrics {
            meta_random_node_lookup_triggered,
            meta_node_still_online_check_triggered,
            meta_libp2p_network_info_num_peers,
            meta_libp2p_network_info_num_connections,
            meta_libp2p_network_info_num_connections_pending,
            meta_libp2p_network_info_num_connections_established,
            meta_libp2p_bandwidth_inbound,
            meta_libp2p_bandwidth_outbound,
        }
    }
}
