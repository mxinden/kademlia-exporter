use async_std::task;
use prometheus::{Encoder, Registry, TextEncoder};
use std::error::Error;

mod exporter;

fn main() -> Result<(), Box<dyn Error>> {
    let registry = Registry::new();
    let exporter = exporter::Exporter::new(&registry)?;

    let _metrics_server = std::thread::spawn(move || {
        task::block_on(async {
            let mut app = tide::with_state(registry);
            app.at("/metrics")
                .get(|req: tide::Request<prometheus::Registry>| async move {
                    let mut buffer = vec![];
                    let encoder = TextEncoder::new();
                    let metric_families = req.state().gather();
                    encoder.encode(&metric_families, &mut buffer).unwrap();

                    String::from_utf8(buffer).unwrap()
                });
            app.listen("127.0.0.1:8080").await.unwrap();
            Result::<(), ()>::Ok(())
        })
    });

    // Kick it off.
    let _listening = false;
    task::block_on(exporter);

    Ok(())
}
