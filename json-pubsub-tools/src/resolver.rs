use super::ResolverCmd;
use json_pubsub::{
    config::resolver::Config,
    path::Path,
    resolver::{Auth, ResolverRead, ResolverWrite},
};
use tokio::runtime::Runtime;

pub(crate) fn run(config: Config, cmd: ResolverCmd, auth: Auth) {
    let mut rt = Runtime::new().expect("failed to init runtime");
    rt.block_on(async {
        match cmd {
            ResolverCmd::Resolve { path } => {
                let resolver = ResolverRead::new(config, auth);
                let resolved = resolver.resolve(vec![path]).await.unwrap();
                println!("resolver: {:?}", resolved[0].resolver);
                for (addr, principal) in resolved[0].krb5_spns.iter() {
                    println!("{}: {}", addr, principal);
                }
                for (addr, _) in &resolved[0].addrs {
                    println!("{}", addr);
                }
            }
            ResolverCmd::List { path } => {
                let resolver = ResolverRead::new(config, auth);
                let path = path.unwrap_or_else(|| Path::from("/"));
                for p in resolver.list(path).await.unwrap() {
                    println!("{}", p);
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
