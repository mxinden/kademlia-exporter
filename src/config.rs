use libp2p::core::Multiaddr;
use serde_derive::Deserialize;
use std::path::PathBuf;

#[derive(Clone, Debug, Deserialize)]
pub struct Config {
    pub max_mind_db_path: Option<PathBuf>,
    pub cloud_provider_cidr_db_path: Option<PathBuf>,

    pub dhts: Vec<DhtConfig>,
}

impl Config {
    pub fn from_file(path: PathBuf) -> Self {
        toml::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct DhtConfig {
    pub name: String,
    pub bootnodes: Vec<Multiaddr>,
    pub disjoint_query_paths: bool,
    pub protocol_name: Option<String>,
    pub noise_legacy: bool,
}
