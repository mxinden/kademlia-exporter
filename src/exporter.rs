use client::Client;
use futures::prelude::*;
use futures_timer::Delay;
use libp2p::{
    identify::IdentifyEvent,
    kad::{GetClosestPeersOk, KademliaEvent},
    multiaddr::{Multiaddr, Protocol},
    PeerId,
};
use maxminddb::{geoip2, Reader};
use prometheus::{
    exponential_buckets, CounterVec, HistogramOpts, HistogramTimer, HistogramVec, Opts, Registry,
};
use std::{
    collections::HashMap,
    error::Error,
    net::IpAddr,
    pin::Pin,
    task::{Context, Poll},
    time::Duration,
};

mod client;

const TICK_INTERVAL: Duration = Duration::from_secs(10);

pub(crate) struct Exporter {
    clients: HashMap<String, Client>,
    metrics: Metrics,
    ip_db: Option<Reader<Vec<u8>>>,

    tick: Delay,

    /// Set of in-flight random peer id lookups.
    ///
    /// When a lookup returns the entry is dropped and thus the duratation is
    /// observed through `<HistogramTimer as Drop>::drop`.
    in_flight_lookups: HashMap<PeerId, HistogramTimer>,
}

impl Exporter {
    pub(crate) fn new(
        dhts: Vec<(String, Multiaddr)>,
        ip_db: Option<Reader<Vec<u8>>>,
        registry: &Registry,
    ) -> Result<Self, Box<dyn Error>> {
        let metrics = Metrics::register(registry);

        let clients = dhts
            .into_iter()
            .map(|(name, bootnode)| (name, client::Client::new(bootnode).unwrap()))
            .collect();

        Ok(Exporter {
            clients,
            metrics,
            ip_db,

            tick: futures_timer::Delay::new(TICK_INTERVAL),

            in_flight_lookups: HashMap::new(),
        })
    }

    fn record_event(&mut self, name: String, event: client::Event) {
        match event {
            client::Event::Ping(_) => {
                self.metrics
                    .event_counter
                    .with_label_values(&[&name, "ping", "ping_event"])
                    .inc();
            }
            client::Event::Identify(event) => match *event {
                IdentifyEvent::Error { .. } => {
                    self.metrics
                        .event_counter
                        .with_label_values(&[&name, "identify", "error"])
                        .inc();
                }
                IdentifyEvent::Sent { .. } => {
                    self.metrics
                        .event_counter
                        .with_label_values(&[&name, "identify", "sent"])
                        .inc();
                }
                IdentifyEvent::Received { .. } => {
                    self.metrics
                        .event_counter
                        .with_label_values(&[&name, "identify", "received"])
                        .inc();
                }
            },
            client::Event::Kademlia(event) => {
                match *event {
                    KademliaEvent::BootstrapResult(_) => {
                        self.metrics
                            .event_counter
                            .with_label_values(&[&name, "kad", "bootstrap"])
                            .inc();
                    }
                    KademliaEvent::GetClosestPeersResult(res) => {
                        self.metrics
                            .event_counter
                            .with_label_values(&[&name, "kad", "get_closest_peers"])
                            .inc();

                        let peer_id = PeerId::from_bytes(match res {
                            Ok(GetClosestPeersOk { key, .. }) => key,
                            Err(err) => err.into_key(),
                        })
                        .unwrap();
                        self.in_flight_lookups.remove(&peer_id);
                    }
                    KademliaEvent::GetProvidersResult(_) => {
                        self.metrics
                            .event_counter
                            .with_label_values(&[&name, "kad", "get_providers"])
                            .inc();
                    }
                    KademliaEvent::StartProvidingResult(_) => {
                        self.metrics
                            .event_counter
                            .with_label_values(&[&name, "kad", "start_providing"])
                            .inc();
                    }
                    KademliaEvent::RepublishProviderResult(_) => {
                        self.metrics
                            .event_counter
                            .with_label_values(&[&name, "kad", "republish_provider"])
                            .inc();
                    }
                    KademliaEvent::GetRecordResult(_) => {
                        self.metrics
                            .event_counter
                            .with_label_values(&[&name, "kad", "get_record"])
                            .inc();
                    }
                    KademliaEvent::PutRecordResult(_) => {
                        self.metrics
                            .event_counter
                            .with_label_values(&[&name, "kad", "put_record"])
                            .inc();
                    }
                    KademliaEvent::RepublishRecordResult(_) => {
                        self.metrics
                            .event_counter
                            .with_label_values(&[&name, "kad", "republish_record"])
                            .inc();
                    }
                    KademliaEvent::Discovered { .. } => {
                        self.metrics
                            .event_counter
                            .with_label_values(&[&name, "kad", "discovered"])
                            .inc();
                    }
                    KademliaEvent::RoutingUpdated {
                        old_peer,
                        addresses,
                        ..
                    } => {
                        let country = self
                            .multiaddresses_to_country_code(addresses.iter())
                            .unwrap_or_else(|| "unknown".to_string());

                        // Check if it is a new node, or just an update to a node.
                        if old_peer.is_none() {
                            self.metrics
                                .unique_nodes_discovered
                                .with_label_values(&[&name, &country])
                                .inc();
                        }
                        self.metrics
                            .event_counter
                            .with_label_values(&[&name, "kad", "routing_updated"])
                            .inc();
                    }
                    KademliaEvent::UnroutablePeer { .. } => {
                        self.metrics
                            .event_counter
                            .with_label_values(&[&name, "kad", "unroutable_peer"])
                            .inc();
                    }
                }
            }
        }
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
            return ip_db
                .lookup::<geoip2::City>(ip_address)
                .ok()?
                .country?
                .iso_code;
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

            // Trigger a random lookup for each client.
            for (name, client) in &mut this.clients {
                let random_peer = PeerId::random();
                let timer = this
                    .metrics
                    .random_node_lookup_duration
                    .with_label_values(&[&name])
                    .start_timer();
                client.get_closest_peers(random_peer.clone());
                this.in_flight_lookups.insert(random_peer, timer);
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
    event_counter: CounterVec,
    unique_nodes_discovered: CounterVec,

    random_node_lookup_duration: HistogramVec,
}

impl Metrics {
    fn register(registry: &Registry) -> Metrics {
        let event_counter = CounterVec::new(
            Opts::new(
                "network_behaviour_event",
                "Libp2p network behaviour events.",
            ),
            &["dht", "behaviour", "event"],
        )
        .unwrap();
        registry.register(Box::new(event_counter.clone())).unwrap();

        let unique_nodes_discovered = CounterVec::new(
            Opts::new(
                "unique_nodes_discovered",
                "Unique nodes discovered through the Dht.",
            ),
            &["dht", "country"],
        )
        .unwrap();
        registry
            .register(Box::new(unique_nodes_discovered.clone()))
            .unwrap();

        let random_node_lookup_duration = HistogramVec::new(
            HistogramOpts::new(
                "random_node_lookup_duration",
                "Duration of random node lookups.",
            )
            .buckets(dbg!(exponential_buckets(0.1, 2.0, 10).unwrap())),
            &["dht"],
        )
        .unwrap();
        registry
            .register(Box::new(random_node_lookup_duration.clone()))
            .unwrap();

        Metrics {
            event_counter,
            unique_nodes_discovered,

            random_node_lookup_duration,
        }
    }
}
