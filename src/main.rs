#![feature(ip)]

use async_std::task;
use prometheus::{Encoder, Registry, TextEncoder};
use std::{
    error::Error,
    path::PathBuf,
    sync::{Arc, Mutex},
};
use structopt::StructOpt;

mod cloud_provider_db;
mod config;
mod exporter;

#[derive(Debug, StructOpt)]
#[structopt(
    name = "Kademlia exporter",
    about = "Monitor the state of a Kademlia Dht."
)]
struct Opt {
    #[structopt(long)]
    config_file: PathBuf,
}

fn main() -> Result<(), Box<dyn Error>> {
    env_logger::init();

    let opt = Opt::from_args();
    let config = config::Config::from_file(opt.config_file);

    let (signal, exit) = exit_future::signal();
    let signal = Arc::new(Mutex::new(Some(signal)));

    ctrlc::set_handler(move || {
        if let Some(signal) = signal.lock().unwrap().take() {
            signal.fire().unwrap();
        }
    })
    .unwrap();

    let registry = Registry::new_custom(Some("kademlia_exporter".to_string()), None).unwrap();
    let ip_db = config
        .max_mind_db_path
        .map(|path| maxminddb::Reader::open_readfile(path).expect("Failed to open max mind db."));
    let cloud_provider_db = config
        .cloud_provider_cidr_db_path
        .map(|path| cloud_provider_db::Db::new(path).expect("Failed to parse cloud provider db."));
    let exporter = exporter::Exporter::new(config.dhts, ip_db, cloud_provider_db, &registry)?;

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
