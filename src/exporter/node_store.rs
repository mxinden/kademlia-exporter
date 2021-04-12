use libp2p::PeerId;
use open_metrics_client::metrics::counter::Counter;
use open_metrics_client::metrics::family::Family;
use open_metrics_client::metrics::gauge::Gauge;
use open_metrics_client::registry::Registry;
use std::{
    collections::HashMap,
    convert::TryInto,
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
            .retain(|_, n| (Instant::now() - n.last_seen) < Duration::from_secs(60 * 60 * 12));
        self.metrics
            .meta_offline_nodes_removed
            .get_or_create(&vec![("dht".to_string(), self.dht.clone())])
            .inc_by((length - self.nodes.len()).try_into().unwrap());
    }

    fn update_metrics(&self) {
        let now = Instant::now();

        //
        // Seen within
        //

        let mut nodes_by_time_by_country_and_provider =
            HashMap::<Duration, HashMap<(String, String), u64>>::new();

        // Insert 3h, 6h, ... buckets.
        for factor in &[3, 6, 12] {
            nodes_by_time_by_country_and_provider
                .insert(Duration::from_secs(60 * 60 * *factor), HashMap::new());
        }

        for node in self.nodes.values() {
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

        for (time_barrier, countries) in nodes_by_time_by_country_and_provider {
            let last_seen_within = format!("{:?}h", time_barrier.as_secs() / 60 / 60);

            for ((country, provider), count) in countries {
                self.metrics
                    .nodes_seen_within
                    .get_or_create(&vec![
                        ("dht".to_string(), self.dht.clone()),
                        ("country".to_string(), country.clone()),
                        ("cloud_provider".to_string(), provider.clone()),
                        ("last_seen_within".to_string(), last_seen_within.clone()),
                    ])
                    .set(count);
            }
        }

        //
        // Up since
        //

        let mut nodes_by_time_by_country_and_provider =
            HashMap::<Duration, HashMap<(String, String), u64>>::new();

        // Insert 3h, 6h, ... buckets.
        for factor in &[3, 6, 12, 24, 48, 96] {
            nodes_by_time_by_country_and_provider
                .insert(Duration::from_secs(60 * 60 * *factor), HashMap::new());
        }

        for node in self.nodes.values() {
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

        for (time_barrier, countries) in nodes_by_time_by_country_and_provider {
            let up_since = format!("{:?}h", time_barrier.as_secs() / 60 / 60);

            for ((country, provider), count) in countries {
                self.metrics
                    .nodes_up_since
                    .get_or_create(&vec![
                        ("dht".to_string(), self.dht.clone()),
                        ("country".to_string(), country.clone()),
                        ("cloud_provider".to_string(), provider.clone()),
                        ("up_since".to_string(), up_since.clone()),
                    ])
                    .set(count);
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

#[derive(Clone)]
pub struct Metrics {
    nodes_seen_within: Family<Vec<(String, String)>, Gauge>,
    nodes_up_since: Family<Vec<(String, String)>, Gauge>,

    meta_offline_nodes_removed: Family<Vec<(String, String)>, Counter>,
}

impl Metrics {
    pub fn register(registry: &mut Registry) -> Metrics {
        let nodes_seen_within = Family::default();
        registry.register(
            "nodes_seen_within",
            "Unique nodes discovered within the time bound through the Dht",
            Box::new(nodes_seen_within.clone()),
        );
        // &["dht", "country", "cloud_provider", "last_seen_within"],

        let nodes_up_since = Family::default();
        registry.register(
            "nodes_up_since",
            "Unique nodes discovered through the Dht and up since timebound",
            Box::new(nodes_up_since.clone()),
        );
        // &["dht", "country", "cloud_provider", "up_since"],

        let meta_offline_nodes_removed = Family::default();
        registry.register(
            "meta_offline_nodes_removed",
            "Number of nodes removed due to being offline longer than 12h",
            Box::new(meta_offline_nodes_removed.clone()),
        );
        // &["dht"],

        Metrics {
            nodes_seen_within,
            nodes_up_since,

            meta_offline_nodes_removed,
        }
    }
}
