use libp2p::PeerId;
use prometheus::{CounterVec, GaugeVec, Opts, Registry};
use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

/// Stores information about a set of nodes for a single Dht.
pub struct NodeStore {
    dht: String,
    nodes: HashMap<PeerId, Node>,

    metrics: Metrics,
}

impl NodeStore {
    pub fn new(dht: String, metrics: Metrics) -> Self {
        NodeStore {
            dht,
            nodes: HashMap::new(),
            metrics,
        }
    }

    /// Record observation of a specific node.
    pub fn observed_node(&mut self, node: Node) {
        match self.nodes.get_mut(&node.peer_id) {
            Some(n) => n.merge(node),
            None => {
                self.nodes.insert(node.peer_id.clone(), node);
            }
        }
    }

    pub fn observed_down(&mut self, peer_id: &PeerId) {
        self.nodes.get_mut(peer_id).unwrap().up_since = None;
    }

    pub fn tick(&mut self) {
        self.update_metrics();

        // Remove old offline nodes.
        let length = self.nodes.len();
        self.nodes
            .retain(|_, n| (Instant::now() - n.last_seen) < Duration::from_secs(60 * 60 * 24));
        self.metrics
            .meta_offline_nodes_removed
            .with_label_values(&[&self.dht])
            .inc_by((length - self.nodes.len()) as f64);
    }

    fn update_metrics(&self) {
        let now = Instant::now();

        //
        // Seen within
        //

        let mut nodes_by_time_by_country = HashMap::<Duration, HashMap<String, u64>>::new();

        // Insert 3h, 6h, ... buckets.
        for factor in &[3, 6, 12, 24] {
            nodes_by_time_by_country.insert(Duration::from_secs(60 * 60 * *factor), HashMap::new());
        }

        for node in self.nodes.values() {
            let since_last_seen = now - node.last_seen;
            for (time_barrier, countries) in &mut nodes_by_time_by_country {
                if since_last_seen < *time_barrier {
                    countries
                        .entry(
                            node.country
                                .clone()
                                .unwrap_or_else(|| "unknown".to_string()),
                        )
                        .and_modify(|v| *v += 1)
                        .or_insert(1);
                }
            }
        }

        for (time_barrier, countries) in nodes_by_time_by_country {
            let last_seen_within = format!("{:?}h", time_barrier.as_secs() / 60 / 60);

            for (country, count) in countries {
                self.metrics
                    .nodes_seen_within
                    .with_label_values(&[&self.dht, &country, &last_seen_within])
                    .set(count as f64);
            }
        }

        //
        // Up since
        //

        let mut nodes_by_time_by_country = HashMap::<Duration, HashMap<String, u64>>::new();

        // Insert 3h, 6h, ... buckets.
        for factor in &[3, 6, 12, 24, 48, 96] {
            nodes_by_time_by_country.insert(Duration::from_secs(60 * 60 * *factor), HashMap::new());
        }

        for node in self.nodes.values() {
            let up_since = match node.up_since {
                Some(instant) => instant,
                None => continue,
            };

            for (time_barrier, countries) in &mut nodes_by_time_by_country {
                if Instant::now() - up_since > *time_barrier {
                    countries
                        .entry(
                            node.country
                                .clone()
                                .unwrap_or_else(|| "unknown".to_string()),
                        )
                        .and_modify(|v| *v += 1)
                        .or_insert(1);
                }
            }
        }

        for (time_barrier, countries) in nodes_by_time_by_country {
            let up_since = format!("{:?}h", time_barrier.as_secs() / 60 / 60);

            for (country, count) in countries {
                self.metrics
                    .nodes_up_since
                    .with_label_values(&[&self.dht, &country, &up_since])
                    .set(count as f64);
            }
        }
    }

    pub fn get_peer(&self, peer_id: &PeerId) -> Option<&Node> {
        self.nodes.get(peer_id)
    }

    pub fn iter(&self) -> impl Iterator<Item = &Node> {
        self.nodes.values()
    }
}

pub struct Node {
    pub peer_id: PeerId,
    pub country: Option<String>,
    last_seen: Instant,
    up_since: Option<Instant>,
}

impl Node {
    pub fn new(peer_id: PeerId) -> Self {
        Node {
            peer_id,
            country: None,
            last_seen: Instant::now(),
            up_since: Some(Instant::now()),
        }
    }

    pub fn with_country(mut self, country: String) -> Self {
        self.country = Some(country);
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

#[derive(Clone)]
pub struct Metrics {
    nodes_seen_within: GaugeVec,
    nodes_up_since: GaugeVec,

    meta_offline_nodes_removed: CounterVec,
}

impl Metrics {
    pub fn register(registry: &Registry) -> Metrics {
        let nodes_seen_within = GaugeVec::new(
            Opts::new(
                "nodes_seen_within",
                "Unique nodes discovered within the time bound through the Dht.",
            ),
            &["dht", "country", "last_seen_within"],
        )
        .unwrap();
        registry
            .register(Box::new(nodes_seen_within.clone()))
            .unwrap();

        let nodes_up_since = GaugeVec::new(
            Opts::new(
                "nodes_up_since",
                "Unique nodes discovered through the Dht and up since timebound.",
            ),
            &["dht", "country", "up_since"],
        )
        .unwrap();
        registry.register(Box::new(nodes_up_since.clone())).unwrap();

        let meta_offline_nodes_removed = CounterVec::new(
            Opts::new(
                "meta_offline_nodes_removed",
                "Number of nodes removed due to being offline longer than 24h.",
            ),
            &["dht"],
        )
        .unwrap();
        registry
            .register(Box::new(meta_offline_nodes_removed.clone()))
            .unwrap();

        Metrics {
            nodes_seen_within,
            nodes_up_since,

            meta_offline_nodes_removed,
        }
    }
}
