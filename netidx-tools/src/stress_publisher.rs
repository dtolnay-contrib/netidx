use futures::{prelude::*, select};
use netidx::{
    config::Config,
    path::Path,
    publisher::{BindCfg, Publisher, Value},
    resolver::Auth,
};
use std::{time::{Duration, Instant}, mem};
use tokio::{runtime::Runtime, signal, time};

async fn run_publisher(
    config: Config,
    bcfg: BindCfg,
    delay: u64,
    rows: usize,
    cols: usize,
    auth: Auth,
) {
    let delay = if delay == 0 { None } else { Some(Duration::from_millis(delay)) };
    let publisher =
        Publisher::new(config, auth, bcfg).await.expect("failed to create publisher");
    let mut sent: usize = 0;
    let mut v = 0u64;
    let published = {
        let mut published = Vec::with_capacity(rows * cols);
        for row in 0..rows {
            for col in 0..cols {
                let path = Path::from(format!("/bench/{}/{}", row, col));
                published.push(publisher.publish(path, Value::V64(v)).expect("encode"))
            }
        }
        published
    };
    let mut last_stat = Instant::now();
    let mut batch: usize = 0;
    let one_second = Duration::from_secs(1);
    loop {
        let mut updates = publisher.start_batch();
        v += 1;
        for (i, p) in published.iter().enumerate() {
            p.update(&mut updates, Value::V64(v + i as u64));
            sent += 1;
            batch += 1;
            if batch > 10000 {
                batch = 0;
                mem::replace(&mut updates, publisher.start_batch()).commit(None).await;
                if let Some(delay) = delay {
                    time::sleep(delay).await;
                }
            }
        }
        updates.commit(None).await;
        if let Some(delay) = delay {
            time::sleep(delay).await;
        }
        let now = Instant::now();
        let elapsed = now - last_stat;
        if elapsed > one_second {
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

pub(crate) fn run(
    config: Config,
    bcfg: BindCfg,
    delay: u64,
    rows: usize,
    cols: usize,
    auth: Auth,
) {
    let rt = Runtime::new().expect("failed to init runtime");
    rt.block_on(async {
        run_publisher(config, bcfg, delay, rows, cols, auth).await;
        // Allow the publisher time to send the clear message
        time::sleep(Duration::from_secs(1)).await;
    });
}
