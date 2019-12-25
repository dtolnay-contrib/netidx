use crate::{
    path::Path,
    utils::{BytesWriter, BatchItem, Batched},
    publisher::{ToPublisher, FromPublisher, Id},
    resolver::{Resolver, ReadOnly},
};
use std::{
    mem, io, iter,
    result::Result,
    marker::PhantomData,
    collections::{HashMap, hash_map::Entry},
    net::{SocketAddr, ToSocketAddrs},
    sync::{Arc, Weak, atomic::{Ordering, AtomicBool}},
};
use async_std::{
    prelude::*,
    task,
    net::TcpStream,
};
use fxhash::FxBuildHasher;
use futures::{
    stream,
    channel::{oneshot, mpsc::{self, Sender, UnboundedReceiver, UnboundedSender}},
    sink::SinkExt as FrsSinkExt,
    future::{FutureExt as FrsFutureExt},
};
use rand::Rng;
use futures_codec::{Framed, LengthCodec};
use serde::de::DeserializeOwned;
use failure::Error;
use bytes::{Bytes, BytesMut};
use parking_lot::Mutex;
use smallvec::SmallVec;

#[derive(Debug)]
struct SubscribeRequest {
    path: Path,
    finished: oneshot::Sender<Result<RawSubscription, Error>>,
    con: UnboundedSender<ToCon>,
}

#[derive(Debug)]
enum ToCon {
    Subscribe(SubscribeRequest),
    Unsubscribe(Id),
    Stream {
        id: Id,
        tx: Sender<Bytes>,
        last: bool,
    },
    Last(Id, oneshot::Sender<Bytes>),
    NotifyDead(Id, oneshot::Sender<()>),
}

#[derive(Debug)]
struct RawSubscriptionInner {
    id: Id,
    addr: SocketAddr,
    dead: Arc<AtomicBool>,
    connection: UnboundedSender<ToCon>,
}

impl Drop for RawSubscriptionInner {
    fn drop(&mut self) {
        let _ = self.connection.unbounded_send(ToCon::Unsubscribe(self.id));
    }
}

#[derive(Debug, Clone)]
pub struct RawSubscriptionWeak(Weak<RawSubscriptionInner>);

impl RawSubscriptionWeak {
    pub fn upgrade(&self) -> Option<RawSubscription> {
        Weak::upgrade(&self.0).map(|r| RawSubscription(r))
    }
}

#[derive(Debug, Clone)]
pub struct RawSubscription(Arc<RawSubscriptionInner>);

impl RawSubscription {
    pub fn typed<T: DeserializeOwned>(self) -> Subscription<T> {
        Subscription(self, PhantomData)
    }

    pub fn downgrade(&self) -> RawSubscriptionWeak {
        RawSubscriptionWeak(Arc::downgrade(&self.0))
    }

    /// Get the last published value, or None if the subscription is dead.
    pub async fn last(&self) -> Option<Bytes> {
        let (tx, rx) = oneshot::channel();
        let _ = self.0.connection.unbounded_send(ToCon::Last(self.0.id, tx));
        rx.await.ok()
    }

    /// Return true if the subscription has died.
    pub fn is_dead(&self) -> bool {
        self.0.dead.load(Ordering::Relaxed)
    }

    /// Wait for the subscription to die
    pub async fn dead(&self) {
        let (tx, rx) = oneshot::channel();
        match self.0.connection.unbounded_send(ToCon::NotifyDead(self.0.id, tx)) {
            Err(_) => (),
            Ok(()) => { let _ = rx.await; },
        }
    }

    /// Get a stream of published values. Values will arrive in the
    /// order they are published. No value will be omitted. If
    /// `begin_with_last` is true, then the stream will start with the
    /// last published value at the time `updates` is called, and will
    /// then receive any updated values. Otherwise the stream will
    /// only receive updated values. If you only want to get the last
    /// value one time, it's cheaper to call `last`.
    ///
    /// When the subscription dies the stream will end.
    pub fn updates(&self, begin_with_last: bool) -> impl Stream<Item = Bytes> {
        let (tx, rx) = mpsc::channel(100);
        let m = ToCon::Stream { tx, last: begin_with_last, id: self.0.id };
        let _ = self.0.connection.unbounded_send(m);
        rx
    }
}

/// A typed version of RawSubscription
#[derive(Debug, Clone)]
pub struct Subscription<T: DeserializeOwned>(RawSubscription, PhantomData<T>);

impl<T: DeserializeOwned> Subscription<T> {
    /// Get the `RawSubscription`
    pub fn raw(&self) -> RawSubscription {
        self.0.clone()
    }

    /// Get and decode the last published value.
    pub async fn last(&self) -> Result<T, Error> {
        match self.0.last().await {
            None => Err(format_err!("subscription is dead")),
            Some(buf) => Ok(rmp_serde::decode::from_read(&*buf)?),
        }
    }

    /// See `RawSubscription::is_dead`
    pub fn is_dead(&self) -> bool {
        self.0.is_dead()
    }

    /// See `RawSubscription::dead`
    pub async fn dead(&self) {
        self.0.dead().await
    }

    /// Same as `RawSubscription::updates` but it decodes the value
    pub fn updates(
        &self,
        begin_with_last: bool
    ) -> impl Stream<Item = Result<T, rmp_serde::decode::Error>> {
        self.0.updates(begin_with_last).map(|v| rmp_serde::decode::from_read(&*v))
    }
}

enum SubStatus {
    Subscribed(RawSubscriptionWeak),
    Pending(Vec<oneshot::Sender<Result<RawSubscription, Error>>>),
}

struct SubscriberInner {
    resolver: Resolver<ReadOnly>,
    connections: HashMap<SocketAddr, UnboundedSender<ToCon>, FxBuildHasher>,
    subscribed: HashMap<Path, SubStatus>,
}

#[derive(Clone)]
pub struct Subscriber(Arc<Mutex<SubscriberInner>>);

impl Subscriber {
    pub fn new<T: ToSocketAddrs>(addrs: T) -> Result<Subscriber, Error> {
        Ok(Subscriber(Arc::new(Mutex::new(SubscriberInner {
            resolver: Resolver::<ReadOnly>::new_r(addrs)?,
            connections: HashMap::with_hasher(FxBuildHasher::default()),
            subscribed: HashMap::new(),
        }))))
    }

    /// Subscribe to the specified set of paths.
    ///
    /// Path resolution and subscription are done in parallel, so the
    /// lowest latency per subscription will be achieved with larger
    /// batches.
    ///
    /// In case you are already subscribed to one or more of the paths
    /// in the batch, you will receive a reference to the existing
    /// subscription, no additional messages will be sent.
    ///
    /// It is safe to call this function concurrently with the same or
    /// overlapping sets of paths in the batch, only one subscription
    /// attempt will be made concurrently, and the result of that one
    /// attempt will be given to each concurrent caller upon success
    /// or failure.
    pub async fn subscribe_raw(
        &self, batch: impl IntoIterator<Item = Path>,
    ) -> Vec<(Path, Result<RawSubscription, Error>)> {
        enum St {
            Resolve,
            Subscribing(oneshot::Receiver<Result<RawSubscription, Error>>),
            WaitingOther(oneshot::Receiver<Result<RawSubscription, Error>>),
            Subscribed(RawSubscription),
            Error(Error),
        }
        let paths = batch.into_iter().collect::<Vec<_>>();
        let mut pending: HashMap<Path, St> = HashMap::new();
        let mut r = { // Init
            let mut t = self.0.lock();
            for p in paths.clone() {
                match t.subscribed.entry(p.clone()) {
                    Entry::Vacant(e) => {
                        e.insert(SubStatus::Pending(vec![]));
                        pending.insert(p, St::Resolve);
                    }
                    Entry::Occupied(mut e) => match e.get_mut() {
                        SubStatus::Pending(ref mut v) => {
                            let (tx, rx) = oneshot::channel();
                            v.push(tx);
                            pending.insert(p, St::WaitingOther(rx));
                        }
                        SubStatus::Subscribed(r) => match r.upgrade() {
                            Some(r) => { pending.insert(p, St::Subscribed(r)); }
                            None => {
                                e.insert(SubStatus::Pending(vec![]));
                                pending.insert(p, St::Resolve);
                            }
                        },
                    }
                }
            }
            t.resolver.clone()
        };
        { // Resolve, Connect, Subscribe
            let mut rng = rand::thread_rng();
            let to_resolve =
                pending.iter()
                .filter(|(_, s)| match s { St::Resolve => true, _ => false })
                .map(|(p, _)| p.clone())
                .collect::<Vec<_>>();
            match r.resolve(to_resolve.clone()).await {
                Err(e) => for p in to_resolve {
                    pending.insert(p.clone(), St::Error(
                        format_err!("resolving path: {} failed: {}", p, e)
                    ));
                }
                Ok(addrs) => {
                    let mut t = self.0.lock();
                    for (p, addrs) in to_resolve.into_iter().zip(addrs.into_iter()) {
                        if addrs.len() == 0 {
                            pending.insert(p, St::Error(format_err!("path not found")));
                        } else {
                            let addr = {
                                if addrs.len() == 1 {
                                    addrs[0]
                                } else {
                                    addrs[rng.gen_range(0, addrs.len())]
                                }
                            };
                            let con =
                                t.connections.entry(addr)
                                .or_insert_with(|| {
                                    let (tx, rx) = mpsc::unbounded();
                                    task::spawn(connection(self.clone(), addr, rx));
                                    tx
                                });
                            let (tx, rx) = oneshot::channel();
                            let con_ = con.clone();
                            let r = con.unbounded_send(ToCon::Subscribe(SubscribeRequest {
                                con: con_,
                                path: p.clone(),
                                finished: tx,
                            }));
                            match r {
                                Ok(()) => { pending.insert(p, St::Subscribing(rx)); }
                                Err(e) => {
                                    pending.insert(p, St::Error(Error::from(e)));
                                }
                            }
                        }
                    }
                }
            }
        }
        // Wait
        for (path, st) in pending.iter_mut() {
            match st {
                St::Resolve => unreachable!(),
                St::Subscribed(_) | St::Error(_) => (),
                St::WaitingOther(w) => match w.await {
                    Err(_) => *st = St::Error(format_err!("other side died")),
                    Ok(Err(e)) => *st = St::Error(e),
                    Ok(Ok(raw)) => *st = St::Subscribed(raw),
                }
                St::Subscribing(w) => {
                    let res = match w.await {
                        Err(_) => Err(format_err!("connection died")),
                        Ok(Err(e)) => Err(e),
                        Ok(Ok(raw)) => Ok(raw),
                    };
                    let mut t = self.0.lock();
                    match t.subscribed.entry(path.clone()) {
                        Entry::Vacant(_) => unreachable!(),
                        Entry::Occupied(mut e) => match res {
                            Err(err) => match e.remove() {
                                SubStatus::Subscribed(_) => unreachable!(),
                                SubStatus::Pending(waiters) => {
                                    for w in waiters {
                                        let err = Err(format_err!("{}", err));
                                        let _ = w.send(err);
                                    }
                                    *st = St::Error(err);
                                }
                            }
                            Ok(raw) => {
                                let s = mem::replace(
                                    e.get_mut(),
                                    SubStatus::Subscribed(raw.downgrade())
                                );
                                match s {
                                    SubStatus::Subscribed(_) => unreachable!(),
                                    SubStatus::Pending(waiters) => {
                                        for w in waiters {
                                            let _ = w.send(Ok(raw.clone()));
                                        }
                                        *st = St::Subscribed(raw);
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        paths.into_iter().map(|p| match pending.remove(&p).unwrap() {
            St::Resolve | St::Subscribing(_) | St::WaitingOther(_) => unreachable!(),
            St::Subscribed(raw) => (p, Ok(raw)),
            St::Error(e) => (p, Err(e))
        }).collect()
    }

    /// Subscribe to one path. This is sufficient for a small number
    /// of paths, but if you need to subscribe to a lot of things use
    /// `subscribe_raw`
    pub async fn subscribe<T: DeserializeOwned>(
        &self,
        path: Path
    ) -> Result<Subscription<T>, Error> {
        self.subscribe_raw(iter::once(path)).await.pop().unwrap().1.map(|v| v.typed())
    }
}

struct Sub {
    path: Path,
    streams: SmallVec<[Sender<Bytes>; 4]>,
    deads: SmallVec<[oneshot::Sender<()>; 4]>,
    last: Bytes,
    dead: Arc<AtomicBool>,
}

async fn handle_val(
    subscriptions: &mut HashMap<Id, Sub, FxBuildHasher>,
    next_sub: &mut Option<SubscribeRequest>,
    id: Id,
    addr: SocketAddr,
    msg: Bytes,
) {
    match subscriptions.entry(id) {
        Entry::Occupied(mut e) => {
            let sub = e.get_mut();
            let mut i = 0;
            while i < sub.streams.len() {
                match sub.streams[i].send(msg.clone()).await {
                    Ok(()) => { i += 1; }
                    Err(_) => { sub.streams.remove(i); }
                }
            }
            sub.last = msg;
        }
        Entry::Vacant(e) => if let Some(req) = next_sub.take() {
            let dead = Arc::new(AtomicBool::new(false));
            e.insert(Sub {
                path: req.path,
                last: msg,
                dead: dead.clone(),
                deads: SmallVec::new(),
                streams: SmallVec::new(),
            });
            let s = RawSubscriptionInner { id, addr, dead, connection: req.con };
            let _ = req.finished.send(Ok(RawSubscription(Arc::new(s))));
        }
    }
}

fn unsubscribe(
    sub: Sub,
    id: Id,
    addr: SocketAddr,
    subscribed: &mut HashMap<Path, SubStatus>
) {
    sub.dead.store(true, Ordering::Relaxed);
    match subscribed.entry(sub.path) {
        Entry::Vacant(_) => (),
        Entry::Occupied(e) => match e.get() {
            SubStatus::Pending(_) => (),
            SubStatus::Subscribed(s) => match s.upgrade() {
                None => { e.remove(); }
                Some(s) => if s.0.id == id && s.0.addr == addr { e.remove(); }
            }
        }
    }
}

fn handle_control(
    addr: SocketAddr,
    subscriber: &Subscriber,
    pending: &mut HashMap<Path, SubscribeRequest>,
    subscriptions: &mut HashMap<Id, Sub, FxBuildHasher>,
    next_val: &mut Option<Id>,
    next_sub: &mut Option<SubscribeRequest>,
    msg: &[u8]
) -> Result<(), Error> {
    match rmp_serde::decode::from_read::<&[u8], FromPublisher>(&*msg) {
        Err(e) => return Err(Error::from(e)),
        Ok(FromPublisher::Message(id)) => { *next_val = Some(id); }
        Ok(FromPublisher::NoSuchValue(path)) =>
            if let Some(r) = pending.remove(&path) {
                let _ = r.finished.send(Err(format_err!("no such value")));
            }
        Ok(FromPublisher::Subscribed(path, id)) => match pending.remove(&path) {
            None => return Err(format_err!("unsolicited: {}", path)),
            Some(req) => {
                *next_val = Some(id);
                *next_sub = Some(req);
            }
        }
        Ok(FromPublisher::Unsubscribed(id)) =>
            if let Some(s) = subscriptions.remove(&id) {
                let mut t = subscriber.0.lock();
                unsubscribe(s, id, addr, &mut t.subscribed);
            }
    }
    Ok(())
}

macro_rules! try_brk {
    ($e:expr) => {
        match $e {
            Ok(v) => v,
            Err(e) => break Err(Error::from(e))
        }
    }
}

async fn connection(
    subscriber: Subscriber,
    to: SocketAddr,
    from_sub: UnboundedReceiver<ToCon>
) -> Result<(), Error> {
    #[derive(Debug)]
    enum M {
        FromPub(Option<Result<Bytes, io::Error>>),
        FromSub(Option<BatchItem<ToCon>>),
    }
    let mut from_sub = Batched::new(from_sub, 100_000);
    let mut pending: HashMap<Path, SubscribeRequest> = HashMap::new();
    let mut subscriptions: HashMap<Id, Sub, FxBuildHasher> =
        HashMap::with_hasher(FxBuildHasher::default());
    let mut next_val: Option<Id> = None;
    let mut next_sub: Option<SubscribeRequest> = None;
    let mut con = Framed::new(TcpStream::connect(to).await?, LengthCodec);
    let mut batched = Vec::new();
    let mut buf = BytesMut::new();
    let enc = |buf: &mut BytesMut, m: &ToPublisher| {
        rmp_serde::encode::write_named(&mut BytesWriter(buf), m).map(|()| {
            buf.split().freeze()
        })
    };
    let res = loop {
        let from_pub = con.next().map(|m| M::FromPub(m));
        let from_sub = from_sub.next().map(|m| M::FromSub(m));
        match dbg!(from_pub.race(from_sub).await) {
            M::FromPub(None) => break Err(format_err!("connection closed")),
            M::FromPub(Some(Err(e))) => break Err(Error::from(e)),
            M::FromPub(Some(Ok(msg))) => match next_val.take() {
                Some(id) => {
                    handle_val(&mut subscriptions, &mut next_sub, id, to, msg).await;
                }
                None => {
                    try_brk!(handle_control(
                        to, &subscriber, &mut pending, &mut subscriptions,
                        &mut next_val, &mut next_sub, &*msg
                    ));
                }
            }
            M::FromSub(None) => break Err(format_err!("dropped")),
            M::FromSub(Some(BatchItem::InBatch(ToCon::Subscribe(req)))) => {
                let path = req.path.clone();
                pending.insert(path.clone(), req);
                batched.push(try_brk!(enc(&mut buf, &ToPublisher::Subscribe(path))));
            }
            M::FromSub(Some(BatchItem::InBatch(ToCon::Unsubscribe(id)))) => {
                batched.push(try_brk!(enc(&mut buf, &ToPublisher::Unsubscribe(id))));
            }
            M::FromSub(Some(BatchItem::InBatch(ToCon::Stream { id, mut tx, last }))) => {
                if let Some(sub) = subscriptions.get_mut(&id) {
                    let mut add = true;
                    if last {
                        if let Err(_) = tx.send(sub.last.clone()).await {
                            add = false;
                        }
                    }
                    if add {
                        sub.streams.push(tx);
                    }
                }
            }
            M::FromSub(Some(BatchItem::InBatch(ToCon::Last(id, tx)))) => {
                if let Some(sub) = subscriptions.get(&id) {
                    let _ = tx.send(sub.last.clone());
                }
            }
            M::FromSub(Some(BatchItem::InBatch(ToCon::NotifyDead(id, tx)))) => {
                if let Some(sub) = subscriptions.get_mut(&id) {
                    sub.deads.push(tx);
                }
            }
            M::FromSub(Some(BatchItem::EndBatch)) => if dbg!(batched.len()) > 0 {
                let mut s = stream::iter(batched.drain(..).map(|v| Ok(v)));
                try_brk!(dbg!(con.send_all(&mut s).await));
            }
        }
    };
    let mut t = subscriber.0.lock();
    for (id, sub) in subscriptions {
        unsubscribe(sub, id, to, &mut t.subscribed);
    }
    dbg!(res)
}

#[cfg(test)]
mod test {
    use std::{
        net::SocketAddr,
        time::Duration,
    };
    use async_std::{prelude::*, future, task};
    use futures::channel::oneshot;
    use crate::{
        resolver_server::Server,
        publisher::{Publisher, BindCfg},
        subscriber::Subscriber,
    };

    async fn init_server() -> Server {
        let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        Server::new(addr, 100).await.expect("start server")
    }

    #[test]
    fn publish_subscribe() {
        #[derive(Debug, Clone, Serialize, Deserialize)]
        struct V {
            id: usize,
            v: String,
        };
        task::block_on(async {
            let server = init_server().await;
            let addr = *server.local_addr();
            let (tx, ready) = oneshot::channel();
            task::spawn(async move {
                let publisher = Publisher::new(addr, BindCfg::Local).await.unwrap();
                let vp0 = publisher.publish(
                    "/app/v0".into(),
                    &V {id: 0, v: "foo".into()}
                ).unwrap();
                let vp1 = publisher.publish(
                    "/app/v1".into(),
                    &V {id: 0, v: "bar".into()}
                ).unwrap();
                publisher.flush(None).await.unwrap();
                tx.send(()).unwrap();
                let mut c = 1;
                loop {
                    future::ready(()).delay(Duration::from_millis(100)).await;
                    vp0.update(&V {id: c, v: "foo".into()})
                        .unwrap();
                    vp1.update(&V {id: c, v: "bar".into()})
                        .unwrap();
                    publisher.flush(None).await.unwrap();
                    c += 1
                }
            });
            future::timeout(Duration::from_secs(1), ready).await.unwrap().unwrap();
            dbg!(());
            let subscriber = Subscriber::new(addr).unwrap();
            let vs0 = subscriber.subscribe::<V>("/app/v0".into()).await.unwrap();
            let vs1 = subscriber.subscribe::<V>("/app/v1".into()).await.unwrap();
            let mut c0: Option<usize> = None;
            let mut c1: Option<usize> = None;
            let mut vs0s = vs0.updates(true);
            let mut vs1s = vs1.updates(true);
            loop {
                match dbg!(vs0s.next().race(vs1s.next()).await) {
                    None => panic!("publishers died"),
                    Some(Err(e)) => panic!("publisher error: {}", e),
                    Some(Ok(v)) => {
                        let c = match &*v.v {
                            "foo" => &mut c0,
                            "bar" => &mut c1,
                            _ => panic!("unexpected v"),
                        };
                        match c {
                            None => { *c = Some(v.id); },
                            Some(c_id) => {
                                assert_eq!(*c_id + 1, v.id);
                                if *c_id >= 50 {
                                    break;
                                }
                                *c = Some(v.id);
                            }
                        }
                    }
                }
            }
            drop(server);
        });
    }
}
