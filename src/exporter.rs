use client::Client;
use futures::prelude::*;
use libp2p::{multiaddr::{Multiaddr, Protocol}, identify::IdentifyEvent, kad::KademliaEvent};

use prometheus::{CounterVec, GaugeVec, Opts, Registry};
use std::{
    collections::HashMap,
    error::Error,
    pin::Pin,
    task::{Context, Poll},
};
use maxminddb::{geoip2, Reader};

mod client;

pub(crate) struct Exporter {
    clients: HashMap<String, Client>,
    metrics: Metrics,
    ip_db: Option<Reader<Vec<u8>>>,
}
impl Exporter {
    pub(crate) fn new(dhts: Vec<Multiaddr>, ip_db: Option<Reader<Vec<u8>>>, registry: &Registry) -> Result<Self, Box<dyn Error>> {
        let metrics = Metrics::register(registry);

        let clients = dhts
            .into_iter()
            .map(|addr| {
                (
                    addr.iter().next().unwrap().to_string(),
                    client::Client::new(addr).unwrap(),
                )
            })
            .collect();


        Ok(Exporter { clients, metrics, ip_db })
    }

    fn record_event(&self, name: String, event: client::Event) {
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
                    KademliaEvent::GetClosestPeersResult(_) => {
                        self.metrics
                            .event_counter
                            .with_label_values(&[&name, "kad", "get_closest_peers"])
                            .inc();
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
                    KademliaEvent::RoutingUpdated { old_peer, addresses, .. } => {
                        let country = self.multiaddresses_to_country_code(addresses.iter()).unwrap_or("unknown".to_string());

                        // Check if it is a new node, or just an update to a node.
                        if old_peer.is_none() {
                            self.metrics.nodes.with_label_values(&[&name, &country]).inc();
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

    fn multiaddresses_to_country_code<'a>(&self, addresses: impl Iterator<Item = &'a Multiaddr>) -> Option<String> {
        for address in addresses {
            let country = self.multiaddress_to_country_code(address);
            if country.is_some() {
                return country
            }
        }

        None
    }

    fn multiaddress_to_country_code(&self, address: &Multiaddr) -> Option<String> {
        match address.iter().next()? {
            Protocol::Ip4(addr) => {
                if let Some(ip_db) = &self.ip_db {
                    return ip_db.lookup::<geoip2::City>(std::net::IpAddr::V4(addr)).ok()?.country?.iso_code;
                }
            }
            _ => {},
        }

        None
    }
}

impl Future for Exporter {
    type Output = ();
    fn poll(mut self: Pin<&mut Self>, ctx: &mut Context) -> Poll<Self::Output> {
        let mut events = vec![];

        for (name, client) in &mut self.clients {
            loop {
                match client.poll_next_unpin(ctx) {
                    Poll::Ready(Some(event)) => events.push((name.clone(), event)),
                    Poll::Ready(None) => return Poll::Ready(()),
                    Poll::Pending => break,
                }
            }
        }

        for (name, event) in events {
            self.record_event(name, event);
        }

        Poll::Pending
    }
}

struct Metrics {
    event_counter: CounterVec,
    nodes: GaugeVec,
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

        let nodes = GaugeVec::new(
            Opts::new("nodes", "Unique nodes discovered through the Dht."),
            &["dht", "country"],
        )
        .unwrap();
        registry.register(Box::new(nodes.clone())).unwrap();

        Metrics {
            event_counter,
            nodes,
        }
    }
}
