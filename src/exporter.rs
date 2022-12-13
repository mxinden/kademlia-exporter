use crate::{cloud_provider_db, config::Config};
use client::Client;
use futures::prelude::*;
use futures_timer::Delay;
use libp2p::{
    identify,
    kad::KademliaEvent,
    multiaddr::{Multiaddr, Protocol},
    ping, PeerId,
};
use log::info;
use maxminddb::{geoip2, Reader};
use node_store::{Node, NodeStore};
use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::gauge::Gauge;
use prometheus_client::registry::Registry;
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
    client: Client,
    node_store: NodeStore,
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
    nodes_to_probe_periodically: Vec<PeerId>,
}

impl Exporter {
    pub(crate) fn new(
        config: Config,
        ip_db: Option<Reader<Vec<u8>>>,
        cloud_provider_db: Option<cloud_provider_db::Db>,
        registry: &mut Registry,
    ) -> Result<Self, Box<dyn Error>> {
        let client = client::Client::new(config, registry).unwrap();

        let sub_registry = registry.sub_registry_with_prefix("kademlia_exporter");

        let metrics = Metrics::register(sub_registry);

        let node_store = NodeStore::default();
        sub_registry.register_collector(Box::new(node_store.clone()));

        Ok(Exporter {
            client,
            metrics,
            ip_db,
            cloud_provider_db,
            node_store,

            tick: futures_timer::Delay::new(TICK_INTERVAL),

            in_flight_lookups: HashMap::new(),
            nodes_to_probe_periodically: vec![],
        })
    }

    fn record_event(&mut self, event: client::ClientEvent) {
        match event {
            client::ClientEvent::Behaviour(client::MyBehaviourEvent::Ping(ping::Event {
                peer,
                result,
            })) => {
                // Update node store.
                match result {
                    Ok(_) => self.node_store.observed_node(Node::new(peer.clone())),
                    Err(_) => self.node_store.observed_down(&peer),
                }
            }
            client::ClientEvent::Behaviour(client::MyBehaviourEvent::Identify(event)) => {
                match event {
                    identify::Event::Error { .. } => {}
                    identify::Event::Sent { .. } => {}
                    identify::Event::Received { peer_id, info } => {
                        self.observe_with_address(peer_id, info.listen_addrs.clone());
                        self.node_store.observed_node(Node::new(peer_id));
                    }
                    identify::Event::Pushed { .. } => {
                        unreachable!("Exporter never pushes identify information.")
                    }
                }
            }
            client::ClientEvent::Behaviour(client::MyBehaviourEvent::Kademlia(event)) => {
                match event {
                    KademliaEvent::RoutablePeer { peer, address } => {
                        self.observe_with_address(peer, vec![address]);
                    }
                    KademliaEvent::PendingRoutablePeer { peer, address } => {
                        self.observe_with_address(peer, vec![address]);
                    }
                    KademliaEvent::RoutingUpdated {
                        peer, addresses, ..
                    } => {
                        self.observe_with_address(peer, addresses.into_vec());
                    }
                    _ => {}
                }
            }
            client::ClientEvent::Behaviour(client::MyBehaviourEvent::KeepAlive(v)) => {
                void::unreachable(v)
            }
            client::ClientEvent::AllConnectionsClosed(peer_id) => {
                self.node_store.observed_down(&peer_id);
            }
        }
    }

    fn observe_with_address(&mut self, peer: PeerId, addresses: Vec<Multiaddr>) {
        let mut node = Node::new(peer);
        if let Some(country) = self.multiaddresses_to_country_code(addresses.iter()) {
            node = node.with_country(country);
        }
        if let Some(provider) = self.multiaddresses_to_cloud_provider(addresses.iter()) {
            node = node.with_cloud_provider(provider);
        }
        self.node_store.observed_node(node);
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

            match this.nodes_to_probe_periodically.pop() {
                Some(peer_id) => {
                    info!("Checking if {:?} is still online.", &peer_id);
                    match this.client.dial(&peer_id) {
                        // New connection was established.
                        Ok(true) => {
                            this.metrics.meta_node_still_online_check_triggered.inc();
                        }
                        // Already connected to node.
                        Ok(false) => {}
                        // Connection limit reached. Retry later.
                        Err(_) => this.nodes_to_probe_periodically.insert(0, peer_id),
                    }
                }
                // List is empty. Reconnected to every peer. Refill the
                // list.
                None => {
                    this.nodes_to_probe_periodically
                        .append(&mut this.node_store.peers());
                }
            }

            // Trigger a random lookup.
            this.metrics.meta_random_node_lookup_triggered.inc();
            let random_peer = PeerId::random();
            this.client.get_closest_peers(random_peer.clone());
            this.in_flight_lookups.insert(random_peer, Instant::now());

            let info = this.client.network_info();
            this.metrics
                .meta_libp2p_network_info_num_peers
                .set(info.num_peers().try_into().unwrap());
            this.metrics
                .meta_libp2p_network_info_num_connections
                .set(info.connection_counters().num_connections().into());
            this.metrics
                .meta_libp2p_network_info_num_connections_established
                .set(info.connection_counters().num_established().into());
            this.metrics
                .meta_libp2p_network_info_num_connections_pending
                .set(info.connection_counters().num_pending().into());
            this.metrics
                .meta_libp2p_bandwidth_inbound
                .set(this.client.total_inbound() as i64);
            this.metrics
                .meta_libp2p_bandwidth_outbound
                .set(this.client.total_outbound() as i64);
        }

        loop {
            match this.client.poll_next_unpin(ctx) {
                Poll::Ready(Some(event)) => this.record_event(event),
                Poll::Ready(None) => return Poll::Ready(()),
                Poll::Pending => break,
            }
        }

        Poll::Pending
    }
}

struct Metrics {
    meta_random_node_lookup_triggered: Counter,
    meta_node_still_online_check_triggered: Counter,
    meta_libp2p_network_info_num_peers: Gauge,
    meta_libp2p_network_info_num_connections: Gauge,
    meta_libp2p_network_info_num_connections_pending: Gauge,
    meta_libp2p_network_info_num_connections_established: Gauge,
    meta_libp2p_bandwidth_inbound: Gauge,
    meta_libp2p_bandwidth_outbound: Gauge,
}

impl Metrics {
    fn register(registry: &mut Registry) -> Metrics {
        let meta_random_node_lookup_triggered = Counter::default();
        registry.register(
            "meta_random_node_lookup_triggered",
            "Number of times a random Kademlia node lookup was triggered",
            meta_random_node_lookup_triggered.clone(),
        );

        let meta_node_still_online_check_triggered = Counter::default();
        registry.register(
            "meta_node_still_online_check_triggered",
            "Number of times a connection to a node was established to ensure it is still online",
            meta_node_still_online_check_triggered.clone(),
        );

        let meta_libp2p_network_info_num_peers = Gauge::default();
        registry.register(
            "meta_libp2p_network_info_num_peers",
            "The total number of connected peers",
            meta_libp2p_network_info_num_peers.clone(),
        );

        let meta_libp2p_network_info_num_connections = Gauge::default();
        registry.register(
            "meta_libp2p_network_info_num_connections",
            "The total number of connections, both established and pending",
            meta_libp2p_network_info_num_peers.clone(),
        );

        let meta_libp2p_network_info_num_connections_pending = Gauge::default();
        registry.register(
            "meta_libp2p_network_info_num_connections_pending",
            "The total number of pending connections, both incoming and outgoing",
            meta_libp2p_network_info_num_connections_pending.clone(),
        );

        let meta_libp2p_network_info_num_connections_established = Gauge::default();
        registry.register(
            "meta_libp2p_network_info_num_connections_established",
            "The total number of established connections",
            meta_libp2p_network_info_num_connections_established.clone(),
        );

        let meta_libp2p_bandwidth_inbound = Gauge::default();
        registry.register(
            "meta_libp2p_bandwidth_inbound",
            "The total number of bytes received on the socket",
            meta_libp2p_bandwidth_inbound.clone(),
        );

        let meta_libp2p_bandwidth_outbound = Gauge::default();
        registry.register(
            "meta_libp2p_bandwidth_outbound",
            "The total number of bytes sent on the socket",
            meta_libp2p_bandwidth_outbound.clone(),
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
