use super::ResolverCmd;
use netidx::{
    chars::Chars,
    config::Config,
    path::Path,
    protocol::glob::{Glob, GlobSet},
    resolver::{Auth, ChangeTracker, ResolverRead, ResolverWrite},
};
use std::{collections::HashSet, iter, time::Duration};
use tokio::{runtime::Runtime, time};
use arcstr::ArcStr;

pub(crate) fn run(config: Config, cmd: ResolverCmd, auth: Auth) {
    let rt = Runtime::new().expect("failed to init runtime");
    rt.block_on(async {
        match cmd {
            ResolverCmd::Resolve { path } => {
                let resolver = ResolverRead::new(config, auth);
                let resolved = resolver.resolve(vec![path]).await.unwrap();
                println!("resolver: {:?}", resolved[0].resolver);
                for (addr, principal) in resolved[0].krb5_spns.iter() {
                    println!("{}: {}", addr, principal);
                }
                for (addr, _) in resolved[0].addrs.iter() {
                    println!("{}", addr);
                }
            }
            ResolverCmd::List { watch, no_structure, path } => {
                let resolver = ResolverRead::new(config, auth);
                let pat = {
                    let path =
                        path.map(|p| Path::from(ArcStr::from(p))).unwrap_or(Path::root());
                    if !Glob::is_glob(&*path) {
                        path.append("*")
                    } else {
                        path
                    }
                };
                let glob = Glob::new(Chars::from(String::from(&*pat))).unwrap();
                let mut ct = ChangeTracker::new(Path::from(ArcStr::from(glob.base())));
                let globs = GlobSet::new(no_structure, iter::once(glob)).unwrap();
                let mut paths = HashSet::new();
                loop {
                    if resolver.check_changed(&mut ct).await.unwrap() {
                        for b in resolver.list_matching(&globs).await.unwrap().iter() {
                            for p in b.iter() {
                                if !paths.contains(p) {
                                    paths.insert(p.clone());
                                    println!("{}", p);
                                }
                            }
                        }
                    }
                    if watch {
                        time::sleep(Duration::from_secs(5)).await
                    } else {
                        break;
                    }
                }
            }
            ResolverCmd::Table { path } => {
                let resolver = ResolverRead::new(config, auth);
                let path = path.unwrap_or_else(|| Path::from("/"));
                let desc = resolver.table(path).await.unwrap();
                println!("columns:");
                for (name, count) in desc.cols.iter() {
                    println!("{}: {}", name, count.0)
                }
                println!("rows:");
                for row in desc.rows.iter() {
                    println!("{}", row);
                }
            }
            ResolverCmd::Add { path, socketaddr } => {
                let resolver = ResolverWrite::new(config, auth, socketaddr);
                resolver.publish(vec![path]).await.unwrap();
            }
            ResolverCmd::Remove { path, socketaddr } => {
                let resolver = ResolverWrite::new(config, auth, socketaddr);
                resolver.unpublish(vec![path]).await.unwrap();
            }
        }
    });
}
