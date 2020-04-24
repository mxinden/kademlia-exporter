#![feature(ip)]

use async_std::task;
use libp2p::core::Multiaddr;
use prometheus::{Encoder, Registry, TextEncoder};
use std::{
    error::Error,
    path::PathBuf,
    sync::{Arc, Mutex},
};
use structopt::StructOpt;

mod cloud_provider_db;
mod exporter;

#[derive(Debug, StructOpt)]
#[structopt(
    name = "Kademlia exporter",
    about = "Monitor the state of a Kademlia Dht."
)]
struct Opt {
    #[structopt(long)]
    dht_name: Vec<String>,
    #[structopt(long)]
    dht_bootnode: Vec<Multiaddr>,

    #[structopt(long)]
    max_mind_db: Option<PathBuf>,
    #[structopt(long)]
    cloud_provider_db: Option<PathBuf>,
}

fn main() -> Result<(), Box<dyn Error>> {
    let opt = Opt::from_args();
    if opt.dht_name.len() != opt.dht_bootnode.len() {
        panic!("Expected equal amount of bootnode and name arguments.");
    }

    env_logger::init();

    let (signal, exit) = exit_future::signal();
    let signal = Arc::new(Mutex::new(Some(signal)));

    ctrlc::set_handler(move || {
        if let Some(signal) = signal.lock().unwrap().take() {
            signal.fire().unwrap();
        }
    })
    .unwrap();

    let registry = Registry::new_custom(Some("kademlia_exporter".to_string()), None).unwrap();
    let ip_db = opt
        .max_mind_db
        .map(|path| maxminddb::Reader::open_readfile(path).expect("Failed to open max mind db."));
    let cloud_provider_db = opt
        .cloud_provider_db
        .map(|path| cloud_provider_db::Db::new(path).expect("Failed to parse cloud provider db."));
    let exporter = exporter::Exporter::new(
        opt.dht_name
            .into_iter()
            .zip(opt.dht_bootnode.into_iter())
            .collect(),
        ip_db,
        cloud_provider_db,
        &registry,
    )?;

    let exit_clone = exit.clone();
    let metrics_server = std::thread::spawn(move || {
        let mut app = tide::with_state(registry);
        app.at("/metrics")
            .get(|req: tide::Request<prometheus::Registry>| async move {
                let mut buffer = vec![];
                let encoder = TextEncoder::new();
                let metric_families = req.state().gather();
                encoder.encode(&metric_families, &mut buffer).unwrap();

                String::from_utf8(buffer).unwrap()
            });
        let endpoint = app.listen("0.0.0.0:8080");
        futures::pin_mut!(endpoint);
        task::block_on(exit_clone.until(endpoint))
    });

    task::block_on(exit.until(exporter));

    metrics_server.join().unwrap();
    Ok(())
}
