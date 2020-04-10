use client::Client;
use futures::{prelude::*, ready};
use libp2p::{identify::IdentifyEvent, kad::KademliaEvent, mdns::MdnsEvent};
use prometheus::{CounterVec, Gauge, Opts, Registry};
use std::{
    error::Error,
    pin::Pin,
    task::{Context, Poll},
};

mod client;

pub(crate) struct Exporter {
    client: Client,
    metrics: Metrics,
}
impl Exporter {
    pub(crate) fn new(registry: &Registry) -> Result<Self, Box<dyn Error>> {
        let metrics = Metrics::register(registry);

        Ok(Exporter {
            client: Client::new()?,
            metrics,
        })
    }

    fn record_event(&self, event: client::Event) {
        match event {
            client::Event::Mdns(event) => match *event {
                MdnsEvent::Discovered(_) => {
                    self.metrics
                        .event_counter
                        .with_label_values(&["mdns", "discovered"])
                        .inc();
                }
                MdnsEvent::Expired(_) => {
                    self.metrics
                        .event_counter
                        .with_label_values(&["mdns", "expired"])
                        .inc();
                }
            },
            client::Event::Ping(_) => {
                self.metrics
                    .event_counter
                    .with_label_values(&["ping", "ping_event"])
                    .inc();
            }
            client::Event::Identify(event) => match *event {
                IdentifyEvent::Error { .. } => {
                    self.metrics
                        .event_counter
                        .with_label_values(&["identify", "error"])
                        .inc();
                }
                IdentifyEvent::Sent { .. } => {
                    self.metrics
                        .event_counter
                        .with_label_values(&["identify", "sent"])
                        .inc();
                }
                IdentifyEvent::Received { .. } => {
                    self.metrics
                        .event_counter
                        .with_label_values(&["identify", "received"])
                        .inc();
                }
            },
            client::Event::Kademlia(event) => {
                match event {
                    KademliaEvent::BootstrapResult(_) => {
                        self.metrics
                            .event_counter
                            .with_label_values(&["kad", "bootstrap"])
                            .inc();
                    }
                    KademliaEvent::GetClosestPeersResult(_) => {
                        self.metrics
                            .event_counter
                            .with_label_values(&["kad", "get_closest_peers"])
                            .inc();
                    }
                    KademliaEvent::GetProvidersResult(_) => {
                        self.metrics
                            .event_counter
                            .with_label_values(&["kad", "get_providers"])
                            .inc();
                    }
                    KademliaEvent::StartProvidingResult(_) => {
                        self.metrics
                            .event_counter
                            .with_label_values(&["kad", "start_providing"])
                            .inc();
                    }
                    KademliaEvent::RepublishProviderResult(_) => {
                        self.metrics
                            .event_counter
                            .with_label_values(&["kad", "republish_provider"])
                            .inc();
                    }
                    KademliaEvent::GetRecordResult(_) => {
                        self.metrics
                            .event_counter
                            .with_label_values(&["kad", "get_record"])
                            .inc();
                    }
                    KademliaEvent::PutRecordResult(_) => {
                        self.metrics
                            .event_counter
                            .with_label_values(&["kad", "put_record"])
                            .inc();
                    }
                    KademliaEvent::RepublishRecordResult(_) => {
                        self.metrics
                            .event_counter
                            .with_label_values(&["kad", "republish_record"])
                            .inc();
                    }
                    KademliaEvent::Discovered { .. } => {
                        self.metrics
                            .event_counter
                            .with_label_values(&["kad", "discovered"])
                            .inc();
                    }
                    KademliaEvent::RoutingUpdated { old_peer, .. } => {
                        // Check if it is a new node, or just an update to a node.
                        if old_peer.is_none() {
                            self.metrics.bucket_size.inc();
                        }
                        self.metrics
                            .event_counter
                            .with_label_values(&["kad", "routing_updated"])
                            .inc();
                    }
                    KademliaEvent::UnroutablePeer { .. } => {
                        self.metrics
                            .event_counter
                            .with_label_values(&["kad", "unroutable_peer"])
                            .inc();
                    }
                }
            }
        }
    }
}

impl Future for Exporter {
    type Output = ();
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        loop {
            match ready!(self.client.poll_next_unpin(cx)) {
                Some(event) => self.record_event(event),
                None => return Poll::Ready(()),
            }
        }
    }
}

struct Metrics {
    event_counter: CounterVec,
    bucket_size: Gauge,
}

impl Metrics {
    fn register(registry: &Registry) -> Metrics {
        let event_counter = CounterVec::new(
            Opts::new(
                "network_behaviour_event",
                "Libp2p network behaviour events.",
            )
            .variable_labels(vec!["behaviour".to_string(), "event".to_string()]),
            &["behaviour", "event"],
        )
        .unwrap();
        registry.register(Box::new(event_counter.clone())).unwrap();

        let bucket_size = Gauge::with_opts(Opts::new(
            "kad_kbuckets_size",
            "Libp2p Kademlia K-Buckets size.",
        ))
        .unwrap();
        registry.register(Box::new(bucket_size.clone())).unwrap();
        Metrics {
            event_counter,
            bucket_size,
        }
    }
}
