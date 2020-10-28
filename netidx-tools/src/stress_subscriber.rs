use futures::channel::mpsc;
use futures::{prelude::*, select_biased};
use netidx::{
    config::Config,
    path::Path,
    resolver::{Auth, ResolverRead},
    subscriber::{Event, Subscriber},
};
use std::time::Duration;
use tokio::{
    runtime::Runtime,
    time::{self, Instant},
};

pub(crate) fn run(config: Config, auth: Auth) {
    let rt = Runtime::new().expect("runtime");
    rt.block_on(async {
        let r = ResolverRead::new(config.clone(), auth.clone());
        let table = r.table(Path::from("/bench")).await.expect("table");
        let subscriber = Subscriber::new(config, auth).unwrap();
        let subs = {
            let mut subs = Vec::with_capacity(table.rows.len() * table.cols.len());
            for row in table.rows.iter() {
                for (col, _) in table.cols.iter() {
                    subs.push(subscriber.durable_subscribe(row.append(col)));
                }
            }
            subs
        };
        let (tx, mut vals) = mpsc::channel(3);
        for s in subs.iter() {
            s.updates(true, tx.clone())
        }
        let start = Instant::now();
        let mut last_stat = start;
        let mut total: usize = 0;
        let mut n: usize = 0;
        let mut batch_size: usize = 0;
        let mut nbatches: usize = 0;
        let mut interval = time::interval(Duration::from_secs(1)).fuse();
        loop {
            select_biased! {
                now = interval.next() => match now {
                    None => break,
                    Some(now) => {
                        let elapsed = now - last_stat;
                        let since_start = now - start;
                        let mut subscribed = 0;
                        let mut unsubscribed = 0;
                        for s in subs.iter() {
                            match s.last() {
                                Event::Unsubscribed => {
                                    unsubscribed += 1;
                                }
                                Event::Update(_) => {
                                    subscribed += 1;
                                }
                            }
                        }
                        println!(
                            "sub: {} !sub: {} rx_i: {:.0} rx_a: {:.0} btch_a: {:.0}",
                            subscribed,
                            unsubscribed,
                            n as f64 / elapsed.as_secs_f64(),
                            total as f64 / since_start.as_secs_f64(),
                            batch_size as f64 / nbatches as f64
                        );
                        nbatches = 0;
                        batch_size = 0;
                        n = 0;
                        last_stat = now;
                    }
                },
                batch = vals.next() => match batch {
                    None => break,
                    Some(mut batch) => {
                        batch_size += batch.len();
                        nbatches += 1;
                        for _ in batch.drain(..) {
                            total += 1;
                            n += 1;
                        }
                    }
                }
            }
        }
        future::pending::<()>().await;
    });
}
