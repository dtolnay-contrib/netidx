use crate::{
    auth::{UserInfo, ANONYMOUS},
    channel::Channel,
    chars::Chars,
    config,
    os::{Krb5ServerCtx, ServerCtx},
    pack::Pack,
    pool::{Pool, Pooled},
    protocol::{
        publisher,
        resolver::{
            ClientAuthRead, ClientAuthWrite, ClientHello, ClientHelloWrite, CtxId,
            FromWrite, ReadyForOwnershipCheck, Secret, ServerAuthWrite, ServerHelloRead,
            ServerHelloWrite, ToRead, ToWrite,
        },
    },
    secstore::SecStore,
    shard_resolver_store::Store,
    utils,
};
use anyhow::Result;
use bytes::{Buf, Bytes};
use futures::{channel::oneshot, prelude::*, select_biased};
use log::{debug, info, warn};
use parking_lot::Mutex;
use std::{
    collections::{HashMap, HashSet},
    mem,
    net::SocketAddr,
    sync::Arc,
    time::Duration,
};
use tokio::{
    net::{TcpListener, TcpStream},
    task,
    time::{self, Instant},
};

atomic_id!(CId);

#[derive(Debug, Clone)]
struct CTracker(Arc<Mutex<HashSet<CId>>>);

impl CTracker {
    fn new() -> Self {
        CTracker(Arc::new(Mutex::new(HashSet::new())))
    }

    fn open(&self) -> CId {
        let id = CId::new();
        self.0.lock().insert(id);
        id
    }

    fn close(&self, id: CId) {
        self.0.lock().remove(&id);
    }

    fn num_open(&self) -> usize {
        self.0.lock().len()
    }
}

enum ClientInfo {
    Running(oneshot::Sender<()>),
    CleaningUp(Vec<oneshot::Sender<()>>),
}

#[derive(Clone)]
struct Clinfos(Arc<Mutex<HashMap<SocketAddr, ClientInfo>>>);

lazy_static! {
    static ref WRITE_BATCHES: Pool<Vec<ToWrite>> = Pool::new(5000, 100000);
    static ref READ_BATCHES: Pool<Vec<ToRead>> = Pool::new(5000, 100000);
}

async fn client_loop_write(
    cfg: Arc<config::Config>,
    clinfos: Clinfos,
    ctracker: CTracker,
    connection_id: CId,
    mut store: Store,
    con: Channel<ServerCtx>,
    secstore: Option<SecStore>,
    server_stop: oneshot::Receiver<()>,
    rx_stop: oneshot::Receiver<()>,
    uifo: Arc<UserInfo>,
    write_addr: SocketAddr,
) -> Result<()> {
    let mut con = Some(con);
    let mut server_stop = server_stop.fuse();
    let mut rx_stop = rx_stop.fuse();
    let mut batch = WRITE_BATCHES.take();
    let mut act = false;
    let mut timeout = time::interval_at(Instant::now() + cfg.writer_ttl, cfg.writer_ttl);
    async fn receive_batch(
        con: &mut Option<Channel<ServerCtx>>,
        batch: &mut Vec<ToWrite>,
    ) -> Result<()> {
        match con {
            Some(ref mut con) => con.receive_batch(batch).await,
            None => future::pending().await,
        }
    }
    'main: loop {
        select_biased! {
            _ = server_stop => break Ok(()),
            _ = rx_stop => break Ok(()),
            _ = timeout.tick().fuse() => {
                if act {
                    act = false;
                } else {
                    drop(con);
                    ctracker.close(connection_id);
                    {
                        let mut inner = clinfos.0.lock();
                        match inner.remove(&write_addr) {
                            None => (),
                            Some(ClientInfo::CleaningUp(_)) => unreachable!(),
                            Some(ClientInfo::Running(stop)) => {
                                let _ = stop.send(());
                            }
                        }
                        let state = ClientInfo::CleaningUp(Vec::new());
                        inner.insert(write_addr, state);
                        if let Some(secstore) = secstore {
                            secstore.remove(&write_addr);
                        }
                    }
                    store.handle_clear(uifo.clone(), write_addr).await?;
                    let mut inner = clinfos.0.lock();
                    inner.remove(&write_addr);
                    bail!("write client timed out");
                }
            },
            m = receive_batch(&mut con, &mut *batch).fuse() => match m {
                Err(e) => {
                    batch.clear();
                    con = None;
                    ctracker.close(connection_id);
                    info!("write client loop error reading message: {}", e)
                },
                Ok(()) => {
                    act = true;
                    if batch.len() == 1 && batch[0] == ToWrite::Heartbeat {
                        continue 'main
                    }
                    let c = con.as_mut().unwrap();
                    while let Some((i, _)) =
                        batch.iter().enumerate().find(|(_, m)| *m == &ToWrite::Clear)
                    {
                        debug!("batch {} contains clear at pos {}", batch.len(), i);
                        let rest = batch.split_off(i + 1);
                        for m in batch.drain(..) {
                            match m {
                                ToWrite::Heartbeat => (),
                                ToWrite::Publish(_)
                                    | ToWrite::PublishDefault(_)
                                    | ToWrite::PublishWithFlags(_, _)
                                    | ToWrite::PublishDefaultWithFlags(_, _) =>
                                    c.queue_send(&FromWrite::Published)?,
                                ToWrite::Unpublish(_) =>
                                    c.queue_send(&FromWrite::Unpublished)?,
                                ToWrite::UnpublishDefault(_) =>
                                    c.queue_send(&FromWrite::Unpublished)?,
                                ToWrite::Clear => {
                                    store.handle_clear(
                                        uifo.clone(),
                                        write_addr
                                    ).await?;
                                    c.queue_send(&FromWrite::Unpublished)?
                                }
                            }
                        }
                        c.flush().await?;
                        batch = Pooled::orphan(rest);
                    }
                    if let Err(e) = store.handle_batch_write(
                        Some(c),
                        uifo.clone(),
                        write_addr,
                        batch.drain(..)
                    ).await {
                        warn!("handle_write_batch failed {}", e);
                        con = None;
                        ctracker.close(connection_id);
                        continue 'main;
                    }
                }
            },
        }
    }
}

async fn hello_client_write(
    cfg: Arc<config::Config>,
    clinfos: Clinfos,
    ctracker: CTracker,
    connection_id: CId,
    listen_addr: SocketAddr,
    store: Store,
    mut con: Channel<ServerCtx>,
    server_stop: oneshot::Receiver<()>,
    secstore: Option<SecStore>,
    resolver_id: SocketAddr,
    hello: ClientHelloWrite,
) -> Result<()> {
    info!("hello_write starting negotiation");
    debug!("hello_write client_hello: {:?}", hello);
    async fn send(
        cfg: &Arc<config::Config>,
        con: &mut Channel<ServerCtx>,
        msg: impl Pack,
    ) -> Result<()> {
        Ok(time::timeout(cfg.hello_timeout, con.send_one(&msg)).await??)
    }
    utils::check_addr(hello.write_addr.ip(), &[listen_addr])?;
    let ttl_expired = loop {
        let rx = {
            let mut inner = clinfos.0.lock();
            match inner.get_mut(&hello.write_addr) {
                None => break true,
                Some(ClientInfo::Running(_)) => break false,
                Some(ClientInfo::CleaningUp(waiters)) => {
                    let (tx, rx) = oneshot::channel();
                    waiters.push(tx);
                    rx
                }
            }
        };
        let _ = rx.await;
    };
    let uifo = match hello.auth {
        ClientAuthWrite::Anonymous => {
            let h = ServerHelloWrite {
                ttl: cfg.writer_ttl.as_secs(),
                ttl_expired,
                resolver_id,
                auth: ServerAuthWrite::Anonymous,
            };
            info!("hello_write accepting Anonymous authentication");
            debug!("hello_write sending hello {:?}", h);
            send(&cfg, &mut con, h).await?;
            ANONYMOUS.clone()
        }
        ClientAuthWrite::Reuse => match secstore {
            None => bail!("authentication not supported"),
            Some(ref secstore) => match secstore.get(&hello.write_addr) {
                None => bail!("session not found"),
                Some(ctx) => {
                    let h = ServerHelloWrite {
                        ttl: cfg.writer_ttl.as_secs(),
                        ttl_expired,
                        resolver_id,
                        auth: ServerAuthWrite::Reused,
                    };
                    info!("hello_write reusing krb5 context");
                    debug!("hello_write sending {:?}", h);
                    send(&cfg, &mut con, h).await?;
                    con.set_ctx(ctx.clone()).await;
                    info!("hello_write all traffic now encrypted");
                    secstore.ifo(Some(&ctx.client()?))?
                }
            },
        },
        ClientAuthWrite::Initiate { spn, token } => match secstore {
            None => bail!("authentication not supported"),
            Some(ref secstore) => {
                info!(
                    "hello_write initiating new krb5 context for {:?}",
                    hello.write_addr
                );
                let (ctx, secret, tok) = secstore.create(&token)?;
                let h = ServerHelloWrite {
                    ttl: cfg.writer_ttl.as_secs(),
                    ttl_expired,
                    resolver_id,
                    auth: ServerAuthWrite::Accepted(tok),
                };
                info!("hello_write created context for {:?}", hello.write_addr);
                debug!("hello_write sending {:?}", h);
                send(&cfg, &mut con, h).await?;
                con.set_ctx(ctx.clone()).await;
                info!("hello_write all traffic now encrypted");
                send(&cfg, &mut con, Secret(secret)).await?;
                let _: ReadyForOwnershipCheck =
                    time::timeout(cfg.hello_timeout, con.receive()).await??;
                info!(
                    "hello_write connecting to {:?} for listener ownership check",
                    hello.write_addr
                );
                let mut con: Channel<ServerCtx> = Channel::new(
                    time::timeout(
                        cfg.hello_timeout,
                        TcpStream::connect(hello.write_addr),
                    )
                    .await??,
                );
                time::timeout(cfg.hello_timeout, con.send_one(&1u64)).await??;
                // we will need to select a protocol version here when
                // we have more than one.
                let _version: u64 =
                    time::timeout(cfg.hello_timeout, con.receive()).await??;
                use publisher::Hello as PHello;
                let m = PHello::ResolverAuthenticate(resolver_id, Bytes::new());
                time::timeout(cfg.hello_timeout, con.send_one(&m)).await??;
                match time::timeout(cfg.hello_timeout, con.receive()).await?? {
                    PHello::Anonymous | PHello::Token(_) => {
                        bail!("listener ownership check unexpected response")
                    }
                    PHello::ResolverAuthenticate(_, mut tok) => {
                        if tok.len() < 8 {
                            bail!("listener ownership check buffer short");
                        }
                        let expected = utils::make_sha3_token(
                            Some(tok.get_u64()),
                            &[&(!secret).to_be_bytes()],
                        );
                        if &*tok != &expected[mem::size_of::<u64>()..] {
                            bail!("listener ownership check failed");
                        }
                        let client = ctx.client()?;
                        let uifo = secstore.ifo(Some(&client))?;
                        let spn = spn.unwrap_or(Chars::from(client));
                        info!("hello_write listener ownership check succeeded");
                        secstore.store(hello.write_addr, spn, secret, ctx.clone());
                        uifo
                    }
                }
            }
        },
    };
    let (tx_stop, rx_stop) = oneshot::channel();
    {
        let mut inner = clinfos.0.lock();
        match inner.get_mut(&hello.write_addr) {
            None => {
                inner.insert(hello.write_addr, ClientInfo::Running(tx_stop));
            }
            Some(ClientInfo::Running(cl)) => {
                let cl = mem::replace(cl, tx_stop);
                let _ = cl.send(());
            }
            Some(ClientInfo::CleaningUp(_)) => bail!("unexpected cleaning up"),
        }
    }
    Ok(client_loop_write(
        cfg,
        clinfos,
        ctracker,
        connection_id,
        store.clone(),
        con,
        secstore,
        server_stop,
        rx_stop,
        uifo,
        hello.write_addr,
    )
    .await?)
}

async fn client_loop_read(
    cfg: Arc<config::Config>,
    mut store: Store,
    mut con: Channel<ServerCtx>,
    server_stop: oneshot::Receiver<()>,
    uifo: Arc<UserInfo>,
) -> Result<()> {
    let mut batch = READ_BATCHES.take();
    let mut server_stop = server_stop.fuse();
    let mut act = false;
    let mut timeout = time::interval_at(Instant::now() + cfg.reader_ttl, cfg.reader_ttl);
    loop {
        select_biased! {
            _ = server_stop => break Ok(()),
            _ = timeout.tick().fuse() => {
                if act {
                    act = false;
                } else {
                    bail!("client timed out");
                }
            }
            m = con.receive_batch(&mut batch).fuse() => {
                m?;
                act = true;
                store.handle_batch_read(
                    &mut con,
                    uifo.clone(),
                    batch.drain(..)
                ).await?;
            },
        }
    }
}

async fn hello_client_read(
    cfg: Arc<config::Config>,
    store: Store,
    mut con: Channel<ServerCtx>,
    server_stop: oneshot::Receiver<()>,
    secstore: Option<SecStore>,
    hello: ClientAuthRead,
) -> Result<()> {
    async fn send(
        cfg: &Arc<config::Config>,
        con: &mut Channel<ServerCtx>,
        hello: ServerHelloRead,
    ) -> Result<()> {
        Ok(time::timeout(cfg.hello_timeout, con.send_one(&hello)).await??)
    }
    let uifo = match hello {
        ClientAuthRead::Anonymous => {
            send(&cfg, &mut con, ServerHelloRead::Anonymous).await?;
            ANONYMOUS.clone()
        }
        ClientAuthRead::Reuse(_) => bail!("read session reuse deprecated"),
        ClientAuthRead::Initiate(tok) => match secstore {
            None => bail!("authentication requested but not supported"),
            Some(ref secstore) => {
                let (ctx, _, tok) = secstore.create(&tok)?;
                send(&cfg, &mut con, ServerHelloRead::Accepted(tok, CtxId::new()))
                    .await?;
                con.set_ctx(ctx.clone()).await;
                secstore.ifo(Some(&ctx.client()?))?
            }
        },
    };
    Ok(client_loop_read(cfg, store.clone(), con, server_stop, uifo).await?)
}

async fn hello_client(
    cfg: Arc<config::Config>,
    clinfos: Clinfos,
    ctracker: CTracker,
    connection_id: CId,
    delay_reads: Option<Instant>,
    listen_addr: SocketAddr,
    store: Store,
    s: TcpStream,
    server_stop: oneshot::Receiver<()>,
    secstore: Option<SecStore>,
    id: SocketAddr,
) -> Result<()> {
    s.set_nodelay(true)?;
    let mut con = Channel::new(s);
    time::timeout(cfg.hello_timeout, con.send_one(&1u64)).await??;
    // we will use this to select a protocol version when there is more than one
    let _version: u64 = time::timeout(cfg.hello_timeout, con.receive()).await??;
    let hello: ClientHello = time::timeout(cfg.hello_timeout, con.receive()).await??;
    match hello {
        ClientHello::ReadOnly(hello) => {
            if let Some(t) = delay_reads {
                if Instant::now() < t {
                    bail!("no read clients allowed yet");
                }
            }
            Ok(hello_client_read(cfg, store.clone(), con, server_stop, secstore, hello)
                .await?)
        }
        ClientHello::WriteOnly(hello) => Ok(hello_client_write(
            cfg,
            clinfos,
            ctracker,
            connection_id,
            listen_addr,
            store.clone(),
            con,
            server_stop,
            secstore,
            id,
            hello,
        )
        .await?),
    }
}

async fn server_loop(
    cfg: config::Config,
    permissions: config::PMap,
    delay_reads: bool,
    id: usize,
    stop: oneshot::Receiver<()>,
    ready: oneshot::Sender<SocketAddr>,
) -> Result<SocketAddr> {
    let delay_reads =
        if delay_reads { Some(Instant::now() + cfg.writer_ttl) } else { None };
    let cfg = Arc::new(cfg);
    let ctracker = CTracker::new();
    let clinfos = Clinfos(Arc::new(Mutex::new(HashMap::new())));
    let id = cfg.addrs[id];
    let secstore = match &cfg.auth {
        config::Auth::Anonymous => None,
        config::Auth::Krb5(spns) => {
            Some(SecStore::new(spns[&id].clone(), permissions, &cfg)?)
        }
    };
    let published =
        Store::new(cfg.parent.clone(), cfg.children.clone(), secstore.clone(), id);
    let listener = TcpListener::bind(id).await?;
    let local_addr = listener.local_addr()?;
    let mut stop = stop.fuse();
    let mut client_stops: Vec<oneshot::Sender<()>> = Vec::new();
    let max_connections = cfg.max_connections;
    let _ = ready.send(local_addr);
    loop {
        select_biased! {
            _ = stop => {
                for cl in client_stops.drain(..) {
                    let _ = cl.send(());
                }
                return Ok(local_addr)
            },
            cl = listener.accept().fuse() => match cl {
                Err(e) => warn!("accept failed: {}", e),
                Ok((client, _)) => {
                    let (tx, rx) = oneshot::channel();
                    client_stops.push(tx);
                    let connection_id = ctracker.open();
                    task::spawn({
                        let clinfos = clinfos.clone();
                        let ctracker = ctracker.clone();
                        let published = published.clone();
                        let secstore = secstore.clone();
                        let cfg = cfg.clone();
                        async move {
                            let r = hello_client(
                                cfg,
                                clinfos,
                                ctracker.clone(),
                                connection_id,
                                delay_reads,
                                local_addr,
                                published,
                                client,
                                rx,
                                secstore,
                                id
                            ).await;
                            ctracker.close(connection_id);
                            info!("server_loop client shutting down {:?}", r);
                        }
                    });
                    while ctracker.num_open() > max_connections {
                        time::sleep(Duration::from_millis(10u64)).await;
                    }
                    debug!("I have {} writers", clinfos.0.lock().len())
                }
            },
        }
    }
}

#[derive(Debug)]
pub struct Server {
    stop: Option<oneshot::Sender<()>>,
    local_addr: SocketAddr,
}

impl Drop for Server {
    fn drop(&mut self) {
        if let Some(stop) = mem::replace(&mut self.stop, None) {
            let _ = stop.send(());
        }
    }
}

impl Server {
    pub async fn new(
        cfg: config::Config,
        permissions: config::PMap,
        delay_reads: bool,
        id: usize,
    ) -> Result<Server> {
        let (send_stop, recv_stop) = oneshot::channel();
        let (send_ready, recv_ready) = oneshot::channel();
        let tsk = server_loop(cfg, permissions, delay_reads, id, recv_stop, send_ready);
        let local_addr = select_biased! {
            a = task::spawn(tsk).fuse() => a??,
            a = recv_ready.fuse() => a?,
        };
        Ok(Server { stop: Some(send_stop), local_addr })
    }

    pub fn local_addr(&self) -> &SocketAddr {
        &self.local_addr
    }
}
