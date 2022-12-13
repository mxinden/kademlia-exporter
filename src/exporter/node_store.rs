use libp2p::PeerId;
use prometheus_client::metrics::gauge::{ConstGauge, Gauge};
use prometheus_client::MaybeOwned;
use prometheus_client::{metrics::counter::Counter, registry::Descriptor};
use std::borrow::Cow;
use std::collections::hash_map::Entry;
use std::convert::TryInto;
use std::sync::atomic::AtomicI64;
use std::sync::{Arc, Mutex};
use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

/// Stores information about a set of nodes for a single Dht.
#[derive(Debug, Default, Clone)]
pub struct NodeStore {
    nodes: Arc<Mutex<HashMap<PeerId, Node>>>,

    offline_nodes_removed: Counter,
}

impl NodeStore {
    /// Record observation of a specific node.
    pub fn observed_node(&mut self, node: Node) {
        match self.nodes.lock().unwrap().entry(node.peer_id) {
            Entry::Occupied(mut entry) => entry.get_mut().merge(node),
            Entry::Vacant(entry) => {
                entry.insert(node);
            }
        }
    }

    pub fn observed_down(&mut self, peer_id: &PeerId) {
        if let Some(peer) = self.nodes.lock().unwrap().get_mut(peer_id) {
            peer.up_since = None;
        }
    }

    pub fn peers(&self) -> Vec<PeerId> {
        self.nodes
            .lock()
            .unwrap()
            .values()
            .map(|p| p.peer_id)
            .collect()
    }
}

impl prometheus_client::collector::Collector for NodeStore {
    fn collect<'a>(
        &'a self,
    ) -> Box<
        dyn Iterator<
                Item = (
                    std::borrow::Cow<'a, prometheus_client::registry::Descriptor>,
                    prometheus_client::MaybeOwned<'a, Box<dyn prometheus_client::registry::Metric>>,
                ),
            > + 'a,
    > {
        let now = Instant::now();
        let mut nodes = self.nodes.lock().unwrap();

        // Remove old offline nodes.
        let length = nodes.len();
        nodes.drain_filter(|_, n| (now - n.last_seen) > Duration::from_secs(60 * 60 * 48));
        self.offline_nodes_removed
            .inc_by((length - nodes.len()).try_into().unwrap());

        //
        // Seen within
        //

        let mut nodes_by_time_by_country_and_provider =
            HashMap::<Duration, HashMap<(String, String), i64>>::new();

        // Insert 3h, 6h, ... buckets.
        for factor in &[3, 6, 12, 24, 48, 96] {
            nodes_by_time_by_country_and_provider
                .insert(Duration::from_secs(60 * 60 * *factor), HashMap::new());
        }

        for node in nodes.values() {
            let since_last_seen = now - node.last_seen;
            for (time_barrier, countries) in &mut nodes_by_time_by_country_and_provider {
                if since_last_seen < *time_barrier {
                    countries
                        .entry((
                            node.country
                                .clone()
                                .unwrap_or_else(|| "unknown".to_string()),
                            node.provider
                                .clone()
                                .unwrap_or_else(|| "unknown".to_string()),
                        ))
                        .and_modify(|v| *v += 1)
                        .or_insert(1);
                }
            }
        }

        let nodes_seen_within = nodes_by_time_by_country_and_provider
            .into_iter()
            .map(|(time_barrier, countries)| {
                let last_seen_within = format!("{:?}h", time_barrier.as_secs() / 60 / 60);
                countries
                    .into_iter()
                    .map(move |((country, provider), count)| {
                        (
                            [
                                ("country".to_string(), country.clone()),
                                ("cloud_provider".to_string(), provider.clone()),
                                ("last_seen_within".to_string(), last_seen_within.clone()),
                            ],
                            ConstGauge::new(count),
                        )
                    })
            })
            .flatten();

        let nodes_seen_within: Box<dyn prometheus_client::registry::Metric> =
            Box::new(parking_lot::Mutex::new(nodes_seen_within));

        //
        // Up since
        //

        let mut nodes_by_time_by_country_and_provider =
            HashMap::<Duration, HashMap<(String, String), i64>>::new();

        // Insert 3h, 6h, ... buckets.
        for factor in &[3, 6, 12, 24, 48, 96] {
            nodes_by_time_by_country_and_provider
                .insert(Duration::from_secs(60 * 60 * *factor), HashMap::new());
        }

        for node in nodes.values() {
            // Safeguard in case exporter is behind on probing every nodes
            // uptime.
            if Instant::now() - node.last_seen > Duration::from_secs(60 * 60) {
                continue;
            }

            let up_since = match node.up_since {
                Some(instant) => instant,
                None => continue,
            };

            for (time_barrier, countries) in &mut nodes_by_time_by_country_and_provider {
                if Instant::now() - up_since > *time_barrier {
                    countries
                        .entry((
                            node.country
                                .clone()
                                .unwrap_or_else(|| "unknown".to_string()),
                            node.provider
                                .clone()
                                .unwrap_or_else(|| "unknown".to_string()),
                        ))
                        .and_modify(|v| *v += 1)
                        .or_insert(1);
                }
            }
        }

        let up_since = nodes_by_time_by_country_and_provider
            .into_iter()
            .map(|(time_barrier, countries)| {
                let up_since = format!("{:?}h", time_barrier.as_secs() / 60 / 60);

                countries
                    .into_iter()
                    .map(move |((country, provider), count)| {
                        (
                            [
                                ("country".to_string(), country.clone()),
                                ("cloud_provider".to_string(), provider.clone()),
                                ("up_since".to_string(), up_since.clone()),
                            ],
                            ConstGauge::new(count),
                        )
                    })
            })
            .flatten();

        let nodes_up_since: Box<dyn prometheus_client::registry::Metric> =
            Box::new(parking_lot::Mutex::new(up_since));

        let iter: Box<
            dyn Iterator<
                    Item = (
                        std::borrow::Cow<'a, prometheus_client::registry::Descriptor>,
                        prometheus_client::MaybeOwned<
                            'a,
                            Box<dyn prometheus_client::registry::Metric>,
                        >,
                    ),
                > + 'a,
        > = Box::new(
            [
                (
                    Cow::Owned(Descriptor::new(
                        "nodes_seen_within",
                        "Unique nodes discovered within the time bound through the Dht",
                        None,
                        None,
                        vec![],
                    )),
                    MaybeOwned::Owned(nodes_seen_within),
                ),
                (
                    Cow::Owned(Descriptor::new(
                        "nodes_up_since",
                        "Unique nodes discovered through the Dht and up since timebound",
                        None,
                        None,
                        vec![],
                    )),
                    MaybeOwned::Owned(nodes_up_since),
                ),
                (
                    Cow::Owned(Descriptor::new(
                        "meta_offline_nodes_removed",
                        "Number of nodes removed due to being offline longer than 12h",
                        None,
                        None,
                        vec![],
                    )),
                    MaybeOwned::Owned(Box::new(self.offline_nodes_removed.clone())),
                ),
                (
                    Cow::Owned(Descriptor::new(
                        "meta_nodes_total",
                        "Number of nodes tracked",
                        None,
                        None,
                        vec![],
                    )),
                    MaybeOwned::Owned({
                        let g: Gauge<_, AtomicI64> = Gauge::default();
                        g.set(nodes.len() as i64);
                        Box::new(g)
                    }),
                ),
                (
                    Cow::Owned(Descriptor::new(
                        "meta_nodes_total",
                        "Number of nodes tracked",
                        None,
                        None,
                        vec![],
                    )),
                    MaybeOwned::Owned({
                        let g: Gauge<_, AtomicI64> = Gauge::default();
                        g.set(nodes.len() as i64);
                        Box::new(g)
                    }),
                ),
            ]
            .into_iter(),
        );

        iter
    }
}

#[derive(Debug)]
pub struct Node {
    pub peer_id: PeerId,
    pub country: Option<String>,
    pub provider: Option<String>,
    last_seen: Instant,
    up_since: Option<Instant>,
}

impl Node {
    pub fn new(peer_id: PeerId) -> Self {
        Node {
            peer_id,
            country: None,
            provider: None,
            last_seen: Instant::now(),
            up_since: Some(Instant::now()),
        }
    }

    pub fn with_country(mut self, country: String) -> Self {
        self.country = Some(country);
        self
    }

    pub fn with_cloud_provider(mut self, provider: String) -> Self {
        self.provider = Some(provider);
        self
    }

    fn merge(&mut self, other: Node) {
        self.country = self.country.take().or(other.country);
        self.up_since = self.up_since.take().or(other.up_since);

        if self.last_seen < other.last_seen {
            self.last_seen = other.last_seen;
        }
    }
}
