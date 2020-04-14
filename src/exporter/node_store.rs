use libp2p::PeerId;
use prometheus::{GaugeVec, Opts, Registry};
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

    pub fn update_metrics(&self) {
        let now = Instant::now();

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
                    .nodes
                    .with_label_values(&[&self.dht, &country, &last_seen_within])
                    .set(count as f64);
            }
        }
    }

    pub fn iter(&self) -> impl Iterator<Item = &Node> {
        self.nodes.values()
    }
}

pub struct Node {
    pub peer_id: PeerId,
    country: Option<String>,
    last_seen: Instant,
}

impl Node {
    pub fn new(peer_id: PeerId) -> Self {
        Node {
            peer_id,
            country: None,
            last_seen: Instant::now(),
        }
    }

    pub fn with_country(mut self, country: String) -> Self {
        self.country = Some(country);
        self
    }

    fn merge(&mut self, other: Node) {
        self.country = self.country.take().or(other.country);

        if self.last_seen < other.last_seen {
            self.last_seen = other.last_seen;
        }
    }
}

#[derive(Clone)]
pub struct Metrics {
    nodes: GaugeVec,
}

impl Metrics {
    pub fn register(registry: &Registry) -> Metrics {
        let nodes = GaugeVec::new(
            Opts::new("nodes", "Unique nodes discovered through the Dht."),
            &["dht", "country", "last_seen_within"],
        )
        .unwrap();
        registry.register(Box::new(nodes.clone())).unwrap();

        Metrics { nodes }
    }
}
