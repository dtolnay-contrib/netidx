use futures::{prelude::*, select};
use netidx::{
    config::Config,
    path::Path,
    publisher::{BindCfg, Publisher, Value},
    resolver::Auth,
};
use std::time::{Duration, Instant};
use tokio::{runtime::Runtime, signal, time};

async fn run_publisher(config: Config, bcfg: BindCfg, nvals: usize, auth: Auth) {
    let publisher =
        Publisher::new(config, auth, bcfg).await.expect("failed to create publisher");
    let mut sent: usize = 0;
    let mut v = 0u64;
    let published = (0..nvals)
        .into_iter()
        .map(|i| {
            let path = Path::from(format!("/bench/{}", i));
            publisher.publish(path, Value::V64(v)).expect("encode")
        })
        .collect::<Vec<_>>();
    publisher.flush(None).await.expect("publish");
    let mut last_stat = Instant::now();
    let one_second = Duration::from_secs(1);
    loop {
        v += 1;
        for p in published.iter() {
            p.update(Value::V64(v));
            sent += 1;
        }
        publisher.flush(None).await.expect("flush");
        let now = Instant::now();
        let elapsed = now - last_stat;
        if elapsed > one_second {
            v = 0;
            select! {
                _ = publisher.wait_any_client().fuse() => (),
                _ = signal::ctrl_c().fuse() => break,
            }
            last_stat = now;
            println!("tx: {:.0}", sent as f64 / elapsed.as_secs_f64());
            sent = 0;
        }
    }
}

pub(crate) fn run(config: Config, bcfg: BindCfg, nvals: usize, auth: Auth) {
    let mut rt = Runtime::new().expect("failed to init runtime");
    rt.block_on(async {
        run_publisher(config, bcfg, nvals, auth).await;
        // Allow the publisher time to send the clear message
        time::delay_for(Duration::from_secs(1)).await;
    });
}