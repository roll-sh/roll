//! Streams multiplexed over one link. rolld opens them ([`Role::Opener`]); the gateway accepts
//! them ([`Role::Acceptor`]) and relays each to a TCP connection.
//!
//! Each stream has a [`WINDOW`] of credit each way: a side sends only what the other granted and
//! grants more only as its reader takes bytes, so what a stream holds unread is at most a window,
//! whatever the peer does; a peer that sends past its credit ends the link. Buffers grow with the
//! bytes that arrived, never from a length the peer sent. Frames go out through an unbounded
//! channel to the link's writer, bounded in practice by the credit (data) and by the streams
//! (control).

use crate::proto::{self, Hello, Kind, MAX_DATA, MAX_STREAMS, OPEN_WAIT, Open, STREAM_LIFETIME, WINDOW, Welcome};
use bytes::{Buf, Bytes, BytesMut};
use std::collections::HashMap;
use std::io;
use std::pin::Pin;
use std::sync::{Arc, Mutex, Weak};
use std::task::{Context, Poll, Waker};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::sync::{mpsc, oneshot};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    Opener,
    Acceptor,
}

/// What the application handles itself: the hello, the welcome, and (the gateway) a stream to
/// dial, which it then [`Mux::accept`]s or [`Mux::refuse`]s.
#[derive(Debug)]
pub enum Event {
    Hello(Hello),
    Welcome(Welcome),
    Open { id: u32, open: Open },
}

/// One link's streams.
#[derive(Clone)]
pub struct Mux {
    inner: Arc<Inner>,
}

struct Inner {
    role: Role,
    out: mpsc::UnboundedSender<Bytes>,
    st: Mutex<MuxState>,
}

#[derive(Default)]
struct MuxState {
    streams: HashMap<u32, Arc<Shared>>,
    /// The opener's streams waiting for `opened` or `refused`.
    pending: HashMap<u32, oneshot::Sender<Result<(), String>>>,
    /// The opener's last id; the acceptor's highest id seen (ids only grow).
    last: u32,
    closed: Option<String>,
}

struct Shared {
    id: u32,
    st: Mutex<StreamState>,
}

struct StreamState {
    inbound: BytesMut,
    /// What the peer may still send.
    allowance: usize,
    /// Taken by the reader since the last grant.
    consumed: usize,
    eof: bool,
    /// What this side may still send.
    credit: usize,
    fin_sent: bool,
    ended: Option<String>,
    read_waker: Option<Waker>,
    write_waker: Option<Waker>,
    up: u64,
    down: u64,
}

impl Shared {
    fn new(id: u32) -> Arc<Shared> {
        Arc::new(Shared {
            id,
            st: Mutex::new(StreamState { inbound: BytesMut::new(), allowance: WINDOW, consumed: 0, eof: false, credit: WINDOW, fin_sent: false, ended: None, read_waker: None, write_waker: None, up: 0, down: 0 }),
        })
    }

    fn end(&self, why: &str) {
        let mut st = self.st.lock().unwrap();
        if st.ended.is_none() {
            st.ended = Some(why.to_string());
        }
        wake(&mut st.read_waker);
        wake(&mut st.write_waker);
    }
}

fn wake(w: &mut Option<Waker>) {
    if let Some(w) = w.take() {
        w.wake();
    }
}

impl Mux {
    /// A link's multiplexer and the frames to write to it, in order.
    pub fn new(role: Role) -> (Mux, mpsc::UnboundedReceiver<Bytes>) {
        let (out, rx) = mpsc::unbounded_channel();
        (Mux { inner: Arc::new(Inner { role, out, st: Mutex::new(MuxState::default()) }) }, rx)
    }

    /// Send a frame of stream 0 (hello, welcome).
    pub fn send_control(&self, kind: Kind, body: &impl serde::Serialize) {
        let _ = self.inner.out.send(proto::encode(kind, 0, &serde_json::to_vec(body).unwrap_or_default()));
    }

    /// Take one message from the link. An error is the peer breaking the protocol: close the link.
    pub fn receive(&self, msg: &[u8]) -> Result<Option<Event>, String> {
        let f = proto::decode(msg)?;
        let inner = &self.inner;
        let opener = inner.role == Role::Opener;
        match f.kind {
            Kind::Hello if !opener => Err("only the gateway says hello".into()),
            Kind::Hello => serde_json::from_slice(f.payload).map(|h| Some(Event::Hello(h))).map_err(|e| format!("a bad hello: {e}")),
            Kind::Welcome if opener => Err("only roll welcomes".into()),
            Kind::Welcome => serde_json::from_slice(f.payload).map(|w| Some(Event::Welcome(w))).map_err(|e| format!("a bad welcome: {e}")),
            Kind::Open if opener => Err("only roll opens streams".into()),
            Kind::Open => {
                let open: Open = serde_json::from_slice(f.payload).map_err(|e| format!("a bad open: {e}"))?;
                let mut st = inner.st.lock().unwrap();
                if st.closed.is_some() {
                    return Ok(None);
                }
                if f.stream <= st.last {
                    return Err("stream ids only grow".into());
                }
                st.last = f.stream;
                if st.streams.len() >= MAX_STREAMS {
                    drop(st);
                    let _ = inner.out.send(proto::encode(Kind::Refused, f.stream, format!("the gateway has {MAX_STREAMS} streams open").as_bytes()));
                    return Ok(None);
                }
                st.streams.insert(f.stream, Shared::new(f.stream));
                Ok(Some(Event::Open { id: f.stream, open }))
            }
            Kind::Opened | Kind::Refused if !opener => Err("only the gateway answers an open".into()),
            Kind::Opened | Kind::Refused => {
                let mut st = inner.st.lock().unwrap();
                match st.pending.remove(&f.stream) {
                    Some(tx) => {
                        let answer = if f.kind == Kind::Opened { Ok(()) } else { Err(String::from_utf8_lossy(f.payload).into_owned()) };
                        if answer.is_err() {
                            st.streams.remove(&f.stream);
                        }
                        let _ = tx.send(answer);
                    }
                    // It gave up waiting: end it on the gateway too.
                    None if f.kind == Kind::Opened && !st.streams.contains_key(&f.stream) => {
                        let _ = inner.out.send(proto::encode(Kind::Reset, f.stream, b"roll stopped waiting"));
                    }
                    None => {}
                }
                Ok(None)
            }
            Kind::Data => {
                let Some(s) = self.stream(f.stream) else { return Ok(None) };
                let mut st = s.st.lock().unwrap();
                if f.payload.len() > st.allowance {
                    return Err(format!("stream {} sent past its window", f.stream));
                }
                if st.ended.is_some() || st.eof {
                    return Ok(None);
                }
                st.allowance -= f.payload.len();
                st.down += f.payload.len() as u64;
                st.inbound.extend_from_slice(f.payload);
                wake(&mut st.read_waker);
                Ok(None)
            }
            Kind::Window => {
                let n = u32::from_be_bytes([f.payload[0], f.payload[1], f.payload[2], f.payload[3]]) as usize;
                let Some(s) = self.stream(f.stream) else { return Ok(None) };
                let mut st = s.st.lock().unwrap();
                if n == 0 || st.credit + n > WINDOW {
                    return Err(format!("stream {} granted past its window", f.stream));
                }
                st.credit += n;
                wake(&mut st.write_waker);
                Ok(None)
            }
            Kind::Fin => {
                if let Some(s) = self.stream(f.stream) {
                    let mut st = s.st.lock().unwrap();
                    st.eof = true;
                    wake(&mut st.read_waker);
                }
                Ok(None)
            }
            Kind::Reset => {
                let s = inner.st.lock().unwrap().streams.remove(&f.stream);
                if let Some(s) = s {
                    s.end(&format!("reset: {}", String::from_utf8_lossy(f.payload)));
                }
                Ok(None)
            }
        }
    }

    fn stream(&self, id: u32) -> Option<Arc<Shared>> {
        self.inner.st.lock().unwrap().streams.get(&id).cloned()
    }

    /// Ask the gateway for a stream to `host:port` (rolld). Errors are the gateway's reason, or
    /// the link's.
    pub async fn open(&self, host: &str, port: u16) -> Result<Stream, String> {
        let inner = &self.inner;
        let (tx, rx) = oneshot::channel();
        let shared = {
            let mut st = inner.st.lock().unwrap();
            if let Some(why) = &st.closed {
                return Err(format!("the gateway's link is closed ({why})"));
            }
            if st.streams.len() >= MAX_STREAMS {
                return Err(format!("the gateway has {MAX_STREAMS} streams open: try again"));
            }
            st.last = st.last.checked_add(1).ok_or("the link has used every stream id: it reconnects")?;
            let id = st.last;
            let s = Shared::new(id);
            st.streams.insert(id, s.clone());
            st.pending.insert(id, tx);
            s
        };
        let id = shared.id;
        let body = serde_json::to_vec(&Open { host: host.to_string(), port }).unwrap_or_default();
        let _ = inner.out.send(proto::encode(Kind::Open, id, &body));
        let answer = match tokio::time::timeout(OPEN_WAIT, rx).await {
            Ok(Ok(a)) => a,
            Ok(Err(_)) => Err("the gateway's link closed".into()),
            Err(_) => Err("the gateway didn't answer in 10 s".into()),
        };
        if let Err(why) = answer {
            let mut st = inner.st.lock().unwrap();
            st.pending.remove(&id);
            st.streams.remove(&id);
            return Err(why);
        }
        Ok(self.handle(shared))
    }

    /// Say a stream the gateway dialed is connected, and take it.
    pub fn accept(&self, id: u32) -> Option<Stream> {
        let s = self.stream(id)?;
        let _ = self.inner.out.send(proto::encode(Kind::Opened, id, b""));
        Some(self.handle(s))
    }

    /// Refuse a stream the gateway won't dial, saying why.
    pub fn refuse(&self, id: u32, why: &str) {
        self.inner.st.lock().unwrap().streams.remove(&id);
        let why: String = why.chars().take(400).collect();
        let _ = self.inner.out.send(proto::encode(Kind::Refused, id, why.as_bytes()));
    }

    fn handle(&self, shared: Arc<Shared>) -> Stream {
        // Every stream ends after its lifetime, whatever its reader and writer do.
        let (weak, id) = (Arc::downgrade(&self.inner), shared.id);
        tokio::spawn(async move {
            tokio::time::sleep(STREAM_LIFETIME).await;
            if let Some(inner) = weak.upgrade() {
                inner.end_stream(id, "the stream's 20 s are up");
            }
        });
        Stream { shared, mux: Arc::downgrade(&self.inner) }
    }

    /// End the link: every stream fails, and nothing more opens.
    pub fn close(&self, why: &str) {
        let (streams, pending) = {
            let mut st = self.inner.st.lock().unwrap();
            if st.closed.is_none() {
                st.closed = Some(why.to_string());
            }
            (std::mem::take(&mut st.streams), std::mem::take(&mut st.pending))
        };
        for (_, s) in streams {
            s.end(why);
        }
        for (_, tx) in pending {
            let _ = tx.send(Err(format!("the gateway's link closed ({why})")));
        }
    }

    pub fn is_closed(&self) -> bool {
        self.inner.st.lock().unwrap().closed.is_some()
    }

    /// Streams open now.
    pub fn live(&self) -> usize {
        self.inner.st.lock().unwrap().streams.len()
    }

    /// Bytes received and not yet read, over every stream (at most a window each).
    pub fn buffered(&self) -> usize {
        let streams: Vec<_> = self.inner.st.lock().unwrap().streams.values().cloned().collect();
        streams.iter().map(|s| s.st.lock().unwrap().inbound.len()).sum()
    }
}

impl Inner {
    fn end_stream(&self, id: u32, why: &str) {
        let s = self.st.lock().unwrap().streams.remove(&id);
        if let Some(s) = s {
            s.end(why);
            let _ = self.out.send(proto::encode(Kind::Reset, id, why.as_bytes()));
        }
    }
}

/// One stream: bytes in order both ways, ending with `fin` (shutdown) or a reset.
pub struct Stream {
    shared: Arc<Shared>,
    mux: Weak<Inner>,
}

impl Stream {
    pub fn id(&self) -> u32 {
        self.shared.id
    }

    /// Bytes sent and received so far (for the audit log).
    pub fn counts(&self) -> (u64, u64) {
        let st = self.shared.st.lock().unwrap();
        (st.up, st.down)
    }

    fn send(&self, kind: Kind, payload: &[u8]) -> io::Result<()> {
        let inner = self.mux.upgrade().ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "the gateway's link is gone"))?;
        inner.out.send(proto::encode(kind, self.shared.id, payload)).map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "the gateway's link is gone"))
    }
}

impl AsyncRead for Stream {
    fn poll_read(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        let mut st = self.shared.st.lock().unwrap();
        if !st.inbound.is_empty() {
            let n = buf.remaining().min(st.inbound.len());
            buf.put_slice(&st.inbound[..n]);
            st.inbound.advance(n);
            st.consumed += n;
            // Grant in quarters of the window, so credit flows back without a frame per read.
            if st.consumed >= WINDOW / 4 && st.ended.is_none() && !st.eof {
                let grant = std::mem::take(&mut st.consumed);
                st.allowance += grant;
                drop(st);
                let _ = self.send(Kind::Window, &(grant as u32).to_be_bytes());
            }
            return Poll::Ready(Ok(()));
        }
        if st.eof {
            return Poll::Ready(Ok(()));
        }
        if let Some(why) = &st.ended {
            return Poll::Ready(Err(io::Error::new(io::ErrorKind::ConnectionReset, why.clone())));
        }
        st.read_waker = Some(cx.waker().clone());
        Poll::Pending
    }
}

impl AsyncWrite for Stream {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        let mut st = self.shared.st.lock().unwrap();
        if let Some(why) = &st.ended {
            return Poll::Ready(Err(io::Error::new(io::ErrorKind::BrokenPipe, why.clone())));
        }
        if st.fin_sent {
            return Poll::Ready(Err(io::Error::new(io::ErrorKind::BrokenPipe, "the stream was shut down")));
        }
        if buf.is_empty() {
            return Poll::Ready(Ok(0));
        }
        if st.credit == 0 {
            st.write_waker = Some(cx.waker().clone());
            return Poll::Pending;
        }
        let n = buf.len().min(st.credit).min(MAX_DATA);
        st.credit -= n;
        st.up += n as u64;
        drop(st);
        self.send(Kind::Data, &buf[..n])?;
        Poll::Ready(Ok(n))
    }

    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        let mut st = self.shared.st.lock().unwrap();
        if st.fin_sent || st.ended.is_some() {
            return Poll::Ready(Ok(()));
        }
        st.fin_sent = true;
        drop(st);
        let _ = self.send(Kind::Fin, b"");
        Poll::Ready(Ok(()))
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        let Some(inner) = self.mux.upgrade() else { return };
        let done = {
            let st = self.shared.st.lock().unwrap();
            st.ended.is_some() || (st.fin_sent && st.eof)
        };
        let removed = inner.st.lock().unwrap().streams.remove(&self.shared.id).is_some();
        if removed && !done {
            let _ = inner.out.send(proto::encode(Kind::Reset, self.shared.id, b"closed"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// Two muxes joined in memory: what one writes the other receives. The pumps report the
    /// protocol errors either side saw.
    fn pair() -> (Mux, Mux, mpsc::UnboundedReceiver<Event>) {
        let (a, mut a_out) = Mux::new(Role::Opener);
        let (b, mut b_out) = Mux::new(Role::Acceptor);
        let (events, ev_rx) = mpsc::unbounded_channel();
        let b2 = b.clone();
        tokio::spawn(async move {
            while let Some(m) = a_out.recv().await {
                if let Some(ev) = b2.receive(&m).unwrap() {
                    let _ = events.send(ev);
                }
            }
        });
        let a2 = a.clone();
        tokio::spawn(async move {
            while let Some(m) = b_out.recv().await {
                a2.receive(&m).unwrap();
            }
        });
        (a, b, ev_rx)
    }

    /// The gateway side: echo every stream back, and refuse port 1.
    fn echo(b: Mux, mut events: mpsc::UnboundedReceiver<Event>) {
        tokio::spawn(async move {
            while let Some(Event::Open { id, open }) = events.recv().await {
                if open.port == 1 {
                    b.refuse(id, "not in the allow-list");
                    continue;
                }
                let mut s = b.accept(id).unwrap();
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 4096];
                    loop {
                        match s.read(&mut buf).await {
                            Ok(0) | Err(_) => break,
                            Ok(n) => {
                                if s.write_all(&buf[..n]).await.is_err() {
                                    break;
                                }
                            }
                        }
                    }
                    let _ = s.shutdown().await;
                });
            }
        });
    }

    #[tokio::test]
    async fn bytes_cross_both_ways_and_refusals_carry_their_reason() {
        let (a, b, events) = pair();
        echo(b, events);
        let s = a.open("db.internal", 5432).await.unwrap();
        let big: Vec<u8> = (0..1_000_000u32).map(|i| i as u8).collect();
        let (mut r, mut w) = tokio::io::split(s);
        let sent = big.clone();
        let writer = tokio::spawn(async move {
            w.write_all(&sent).await.unwrap();
            w.shutdown().await.unwrap();
            w
        });
        let mut got = Vec::new();
        r.read_to_end(&mut got).await.unwrap();
        assert!(got == big, "a megabyte through the window, in order");
        let s = r.unsplit(writer.await.unwrap());
        assert_eq!(s.counts(), (1_000_000, 1_000_000));
        drop(s);
        assert_eq!(a.open("10.0.0.1", 1).await.err().unwrap(), "not in the allow-list");
        a.close("bye");
        assert!(a.open("db", 5432).await.err().unwrap().contains("closed"));
    }

    #[tokio::test]
    async fn a_slow_reader_holds_at_most_a_window() {
        let (a, b, mut events) = pair();
        let a2 = a.clone();
        let opening = tokio::spawn(async move { a2.open("db", 5432).await });
        let Some(Event::Open { id, .. }) = events.recv().await else { panic!("no open") };
        let mut g = b.accept(id).unwrap();
        let mut s = opening.await.unwrap().unwrap();
        // The gateway writes up to 4 MiB; roll reads nothing meanwhile.
        let writer = tokio::spawn(async move {
            let chunk = vec![7u8; 64 * 1024];
            let mut sent = 0usize;
            while sent < 4 << 20 {
                match tokio::time::timeout(std::time::Duration::from_millis(300), g.write_all(&chunk)).await {
                    Ok(Ok(())) => sent += chunk.len(),
                    _ => break,
                }
            }
            (sent, g)
        });
        let (sent, g) = writer.await.unwrap();
        assert!(sent <= WINDOW, "the writer stopped at the window ({sent} bytes)");
        assert!(a.buffered() <= WINDOW && b.buffered() == 0);
        // Reading frees credit.
        let mut buf = vec![0u8; WINDOW];
        s.read_exact(&mut buf).await.unwrap();
        drop(g);
        drop(s);
        assert_eq!(a.live(), 0);
    }

    #[tokio::test]
    async fn a_peer_past_its_window_or_its_streams_is_refused() {
        let (a, mut a_out) = Mux::new(Role::Acceptor);
        a.receive(&proto::encode(Kind::Open, 1, br#"{"host":"h","port":5}"#)).unwrap();
        let _s = a.accept(1).unwrap();
        let chunk = vec![0u8; MAX_DATA];
        for _ in 0..WINDOW / MAX_DATA {
            a.receive(&proto::encode(Kind::Data, 1, &chunk)).unwrap();
        }
        assert!(a.receive(&proto::encode(Kind::Data, 1, b"x")).unwrap_err().contains("window"));
        assert!(a.receive(&proto::encode(Kind::Window, 1, &1u32.to_be_bytes())).unwrap_err().contains("window"), "credit can't grow past a window");
        assert!(a.receive(&proto::encode(Kind::Open, 1, br#"{"host":"h","port":5}"#)).unwrap_err().contains("grow"));
        for id in 2..=MAX_STREAMS as u32 {
            assert!(a.receive(&proto::encode(Kind::Open, id, br#"{"host":"h","port":5}"#)).unwrap().is_some());
        }
        while a_out.try_recv().is_ok() {}
        assert!(a.receive(&proto::encode(Kind::Open, 99, br#"{"host":"h","port":5}"#)).unwrap().is_none());
        assert_eq!(proto::decode(&a_out.try_recv().unwrap()).unwrap().kind, Kind::Refused);
    }
}
