#![recursion_limit = "1024"]
#[macro_use]
extern crate serde_derive;
#[macro_use]
extern crate netidx;

use log::warn;
use netidx::{config, path::Path, publisher::BindCfg, resolver::Auth};
use std::net::SocketAddr;
use structopt::StructOpt;

mod publisher;
mod resolver;
mod stress_publisher;
mod stress_subscriber;
mod subscriber;

#[cfg(unix)]
mod resolver_server;

#[cfg(not(unix))]
mod resolver_server {
    use netidx::config;

    pub(crate) fn run(
        _config: config::Config,
        _permissions: config::PMap,
        _daemonize: bool,
        _delay_reads: bool,
        _id: usize,
    ) {
        todo!("the resolver server is not yet ported to this platform")
    }
}

#[derive(StructOpt, Debug)]
#[structopt(name = "json-pubsub")]
struct Opt {
    #[structopt(
        short = "c",
        long = "config",
        help = "override the default config file location (~/.config/netidx.json)"
    )]
    config: Option<String>,
    #[structopt(subcommand)]
    cmd: Sub,
}

#[derive(StructOpt, Debug)]
enum Sub {
    #[structopt(name = "resolver-server", about = "run a resolver")]
    ResolverServer {
        #[structopt(short = "f", long = "foreground", help = "don't daemonize")]
        foreground: bool,
        #[structopt(
            long = "delay-reads",
            help = "don't allow read clients until 1 writer ttl has passed"
        )]
        delay_reads: bool,
        #[structopt(
            long = "id",
            help = "index of the address to bind to",
            default_value = "0"
        )]
        id: usize,
        #[structopt(
            short = "p",
            long = "permissions",
            help = "location of the permissions file"
        )]
        permissions: Option<String>,
    },
    #[structopt(name = "resolver", about = "query the resolver")]
    Resolver {
        #[structopt(short = "k", long = "krb5", help = "use Kerberos 5")]
        krb5: bool,
        #[structopt(long = "upn", help = "krb5 use <upn> instead of the current user")]
        upn: Option<String>,
        #[structopt(subcommand)]
        cmd: ResolverCmd,
    },
    #[structopt(name = "publisher", about = "publish data")]
    Publisher {
        #[structopt(short = "k", long = "krb5", help = "use Kerberos 5")]
        krb5: bool,
        #[structopt(
            short = "b",
            long = "bind",
            help = "configure the bind address e.g. 192.168.0.0/16, 127.0.0.1:5000"
        )]
        bind: BindCfg,
        #[structopt(long = "upn", help = "krb5 use <upn> instead of the current user")]
        upn: Option<String>,
        #[structopt(long = "spn", help = "krb5 use <spn>")]
        spn: Option<String>,
        #[structopt(long = "type", help = "data type (use help for a list)")]
        typ: publisher::Typ,
        #[structopt(
            long = "timeout",
            help = "require subscribers to consume values before timeout (seconds)"
        )]
        timeout: Option<u64>,
    },
    #[structopt(name = "subscriber", about = "subscribe to values")]
    Subscriber {
        #[structopt(short = "k", long = "krb5", help = "use Kerberos 5")]
        krb5: bool,
        #[structopt(long = "upn", help = "krb5 use <upn> instead of the current user")]
        upn: Option<String>,
        #[structopt(name = "paths")]
        paths: Vec<String>,
    },
    #[structopt(name = "stress", about = "stress test")]
    Stress {
        #[structopt(subcommand)]
        cmd: Stress,
    },
}

#[derive(StructOpt, Debug)]
enum ResolverCmd {
    #[structopt(name = "resolve", about = "resolve an in the resolver server")]
    Resolve { path: Path },
    #[structopt(name = "list", about = "list entries in the resolver server")]
    List {
        #[structopt(name = "path")]
        path: Option<Path>,
    },
    #[structopt(name = "add", about = "add a new entry")]
    Add {
        #[structopt(name = "path")]
        path: Path,
        #[structopt(name = "socketaddr")]
        socketaddr: SocketAddr,
    },
    #[structopt(name = "remove", about = "remove an entry")]
    Remove {
        #[structopt(name = "path")]
        path: Path,
        #[structopt(name = "socketaddr")]
        socketaddr: SocketAddr,
    },
}

#[derive(StructOpt, Debug)]
enum Stress {
    #[structopt(name = "publisher", about = "run a stress test publisher")]
    Publisher {
        #[structopt(short = "k", long = "krb5", help = "use Kerberos 5")]
        krb5: bool,
        #[structopt(
            short = "b",
            long = "bind",
            help = "configure the bind address e.g. 192.168.0.0/16, 127.0.0.1:5000"
        )]
        bind: BindCfg,
        #[structopt(long = "upn", help = "krb5 use <upn> instead of the current user")]
        upn: Option<String>,
        #[structopt(long = "spn", help = "krb5 use <spn>")]
        spn: Option<String>,
        #[structopt(name = "nvals", default_value = "100")]
        nvals: usize,
    },
    #[structopt(name = "subscriber", about = "run a stress test subscriber")]
    Subscriber {
        #[structopt(short = "k", long = "krb5", help = "use Kerberos 5")]
        krb5: bool,
        #[structopt(long = "upn", help = "krb5 use <upn> instead of the current user")]
        upn: Option<String>,
    },
}

fn auth(krb5: bool, upn: Option<String>, spn: Option<String>) -> Auth {
    if !krb5 {
        Auth::Anonymous
    } else {
        Auth::Krb5 { upn, spn }
    }
}

fn main() {
    env_logger::init();
    let opt = Opt::from_args();
    let cfg = match opt.config {
        None => config::Config::load_default().unwrap(),
        Some(path) => config::Config::load(path).unwrap(),
    };
    match opt.cmd {
        Sub::ResolverServer { foreground, delay_reads, id, permissions } => {
            if !cfg!(unix) {
                todo!("the resolver server is not yet ported to this platform")
            }
            let anon = match cfg.auth {
                config::Auth::Anonymous => true,
                config::Auth::Krb5(_) => false,
            };
            let permissions = match permissions {
                None if anon => config::PMap::default(),
                None => panic!("--permissions is required when using Kerberos"),
                Some(_) if anon => {
                    warn!("ignoring --permissions, server not using Kerberos");
                    config::PMap::default()
                }
                Some(p) => config::PMap::load(&p).unwrap(),
            };
            resolver_server::run(cfg, permissions, !foreground, delay_reads, id)
        }
        Sub::Resolver { krb5, upn, cmd } => {
            resolver::run(cfg, cmd, auth(krb5, upn, None))
        }
        Sub::Publisher { bind, krb5, upn, spn, typ, timeout } => {
            publisher::run(cfg, bind, typ, timeout, auth(krb5, upn, spn))
        }
        Sub::Subscriber { krb5, upn, paths } => {
            subscriber::run(cfg, paths, auth(krb5, upn, None))
        }
        Sub::Stress { cmd } => match cmd {
            Stress::Subscriber { krb5, upn } => {
                stress_subscriber::run(cfg, auth(krb5, upn, None))
            }
            Stress::Publisher { bind, krb5, upn, spn, nvals } => {
                stress_publisher::run(cfg, bind, nvals, auth(krb5, upn, spn))
            }
        },
    }
}