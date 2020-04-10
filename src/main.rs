use async_std::task;
use prometheus::{Encoder, Registry, TextEncoder};
use std::{error::Error, sync::{Arc, Mutex}};

mod behaviour;
mod exporter;

fn main() -> Result<(), Box<dyn Error>> {
    let (signal, exit) = exit_future::signal();
    let signal = Arc::new(Mutex::new(Some(signal)));

    ctrlc::set_handler(move || {
        println!("Got ctrlc");
        match signal.lock().unwrap().take() {
            Some(signal) => signal.fire().unwrap(),
            None => {}
        }
    }).unwrap();

    let registry = Registry::new();
    let exporter = exporter::Exporter::new(&registry)?;

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
