use async_std::task;
use libp2p::core::Multiaddr;
use prometheus::{Encoder, Registry, TextEncoder};
use std::{
    error::Error,
    sync::{Arc, Mutex},
};
use structopt::StructOpt;

mod exporter;

#[derive(Debug, StructOpt)]
#[structopt(name = "Kademlia exporter", about = "Monitor the state of a Kademlia Dht.")]
struct Opt {
    #[structopt(long)]
    dht: Vec<Multiaddr>,
}

fn main() -> Result<(), Box<dyn Error>> {
    let opt = Opt::from_args();

    env_logger::init();

    let (signal, exit) = exit_future::signal();
    let signal = Arc::new(Mutex::new(Some(signal)));

    ctrlc::set_handler(move || {
        if let Some(signal) = signal.lock().unwrap().take() {
            signal.fire().unwrap();
        }
    })
    .unwrap();

    let registry = Registry::new();
    let exporter = exporter::Exporter::new(opt.dht, &registry)?;

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
