//! Listener and per-connection tasks.
//!
//! Each connection gets two tasks: a reader that parses inbound frames and a
//! writer that owns the socket's write half. Parsing, and later TLS and
//! compression, therefore happen per-connection on worker threads rather than
//! on the single task that owns room state.

use crate::actor::{self, ActorMsg, SaveConfig};
use crate::config::NetConfig;
use crate::shard::{Outbound, Shards};
use crate::ws;
use pahoa_proto::decode;
use pahoa_room::{ConnId, FeedPolicy, Room};
use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;

/// A running server.
pub struct Server {
    pub local_addr: SocketAddr,
    /// Where the scoped feed is listening, if it was asked for.
    pub filtered_addr: Option<SocketAddr>,
    actor_tx: mpsc::Sender<ActorMsg>,
    /// Fires when the actor loop has returned, final save included. Taken by
    /// the first `shutdown` caller; a second call has nothing left to wait for.
    stopped: tokio::sync::Mutex<Option<tokio::sync::oneshot::Receiver<()>>>,
    /// Fired by `POST /admin/v1/shutdown`.
    shutdown_requested: Arc<tokio::sync::Notify>,
    /// The accept loop, so shutting down can stop taking new connections rather
    /// than racing them against a room that is going away.
    accepting: Vec<tokio::task::JoinHandle<()>>,
}

impl Server {
    /// Bind and start serving, without persistence. Returns once the listener
    /// is up, so tests can connect without racing.
    pub async fn start(room: Room, config: NetConfig) -> io::Result<Self> {
        Self::start_with_saves(room, config, SaveConfig::default()).await
    }

    /// As [`Server::start`], persisting to a [`crate::SaveStore`].
    ///
    /// Restoring is the caller's job and happens *before* this: a room is
    /// constructed, restored, and only then served, so no client can ever see
    /// a half-loaded room.
    pub async fn start_with_saves(
        mut room: Room,
        config: NetConfig,
        saves: SaveConfig,
    ) -> io::Result<Self> {
        // Before the listener, on the same reasoning as the save lock: a room
        // configured with an unusable certificate should refuse to start rather
        // than bind and then fail every handshake.
        let tls = match &config.tls {
            None => None,
            Some(paths) => {
                let resolver = crate::tls::CertResolver::load(paths.clone())?;
                crate::tls::spawn_reloader(Arc::clone(&resolver), crate::tls::RELOAD_INTERVAL);
                Some(crate::tls::acceptor(resolver))
            }
        };

        let listener = TcpListener::bind((config.bind.as_str(), config.port)).await?;
        let local_addr = listener.local_addr()?;

        // The scoped feed's listener, if one was asked for. Bound here too, so a
        // port already in use fails at startup rather than leaving a room that
        // serves half of what it advertised.
        let filtered = match config.filtered_port {
            None => None,
            Some(port) => {
                let listener = TcpListener::bind((config.bind.as_str(), port)).await?;
                let addr = listener.local_addr()?;
                Some((listener, addr))
            }
        };
        let filtered_addr = filtered.as_ref().map(|(_, addr)| *addr);

        let budget = crate::budget::Budget::new(
            config.outbound_budget_bytes,
            config.per_connection_budget_bytes,
        );
        let shards = Shards::spawn(
            config.shards_resolved(),
            4096,
            config.compression_level,
            budget.clone(),
        );
        let (actor_tx, actor_rx) = mpsc::channel(8192);

        // Taken before the room moves into the actor. The seed is immutable and
        // already an `Arc`, so the HTTP surface can describe the room without
        // asking the actor for anything that does not change.
        let shutdown_requested = Arc::new(tokio::sync::Notify::new());
        let router = crate::http::Router::new(
            room.multidata_arc(),
            actor_tx.clone(),
            &config,
            Arc::clone(&shutdown_requested),
        );

        let (stopped_tx, stopped_rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            actor::run_with_saves(&mut room, shards, actor_rx, saves).await;
            let _ = stopped_tx.send(());
        });

        // One counter across both listeners: a `ConnId` identifies a connection
        // to the room, and two ports handing out the same one would be two
        // clients the actor could not tell apart.
        let next_id = Arc::new(AtomicU64::new(1));

        let port = Port {
            actor: actor_tx.clone(),
            config,
            tls,
            router,
            feed: FeedPolicy::Full,
        };
        let mut accepting = vec![accept_loop(listener, port.clone(), Arc::clone(&next_id))];
        if let Some((listener, _)) = filtered {
            accepting.push(accept_loop(
                listener,
                Port {
                    feed: FeedPolicy::Scoped,
                    ..port
                },
                next_id,
            ));
        }

        Ok(Self {
            filtered_addr,
            local_addr,
            actor_tx,
            stopped: tokio::sync::Mutex::new(Some(stopped_rx)),
            shutdown_requested,
            accepting,
        })
    }

    /// Resolves when something inside the room asks the process to stop.
    ///
    /// Today that is only `POST /admin/v1/shutdown`. The caller selects on this
    /// alongside SIGINT and SIGTERM, so every route out of a running room takes
    /// the same exit path and the same final save.
    pub async fn shutdown_requested(&self) {
        self.shutdown_requested.notified().await;
    }

    /// Stop the room and wait for it to finish, including its final save.
    ///
    /// Waiting matters: without it the process can exit while the flush is
    /// still on a blocking thread, which throws away the very state the flush
    /// exists to keep. The flush has its own timeout, so this cannot hang
    /// indefinitely on a stuck filesystem.
    pub async fn shutdown(&self) {
        // Stop taking new connections first. A client that arrives during the
        // final save would be told about a room that is already going away, and
        // its `Connect` would race the actor's last message.
        for accepting in &self.accepting {
            accepting.abort();
        }

        let _ = self.actor_tx.send(ActorMsg::Shutdown).await;
        if let Some(stopped) = self.stopped.lock().await.take() {
            let _ = stopped.await;
        }

        // Only now that the save is on disk: give the close frames the room
        // just broadcast a moment to reach the wire. Dropping the runtime aborts
        // the writer tasks, so without this a client's last word from the room
        // is a TCP reset rather than "the room is closing".
        //
        // Deliberately after the flush and deliberately bounded — this must
        // never be able to eat into the save's budget.
        tokio::time::sleep(CLOSE_LINGER).await;
    }
}

/// What the keepalive wants done when its timer comes due.
enum Due {
    /// Send this Ping frame.
    Ping(bytes::Bytes),
    /// No pong arrived inside the timeout. The peer is gone.
    Dead,
    /// The outstanding ping was answered; nothing to do yet.
    Nothing,
}

/// Per-connection liveness, matching the reference's `ping_interval` /
/// `ping_timeout` pair.
///
/// Two independent deadlines rather than one ticker, because they answer
/// different questions: `next_ping` is the keepalive cadence that has to beat a
/// middlebox's idle reaper, and `judge_at` is how long the peer gets to answer.
/// Collapsing them into a single timer would make the effective cadence
/// `interval + timeout` — halving the keepalive rate, which is the half that
/// matters when nothing else is on the wire.
struct Keepalive {
    interval: Duration,
    timeout: Duration,
    /// Bumped by the reader on every Pong. Compared rather than timestamped:
    /// the writer only needs to know whether *any* arrived since it asked.
    pongs: Arc<AtomicU64>,
    next_ping: Option<tokio::time::Instant>,
    /// When to judge the outstanding ping, and the pong count when it was sent.
    judge_at: Option<(tokio::time::Instant, u64)>,
}

impl Keepalive {
    fn new(interval: Duration, timeout: Duration, pongs: Arc<AtomicU64>) -> Self {
        Self {
            interval,
            timeout,
            pongs,
            // Zero disables pinging entirely, which also disables judging:
            // there is nothing outstanding to judge.
            next_ping: (!interval.is_zero()).then(|| tokio::time::Instant::now() + interval),
            judge_at: None,
        }
    }

    /// Resolve when something is due. Never resolves when pings are off.
    async fn wait(&self) {
        let at = match (self.next_ping, self.judge_at) {
            (None, None) => return std::future::pending().await,
            (Some(a), None) => a,
            (None, Some((b, _))) => b,
            (Some(a), Some((b, _))) => a.min(b),
        };
        tokio::time::sleep_until(at).await;
    }

    fn due(&mut self) -> Due {
        let now = tokio::time::Instant::now();

        // Judged first: a ping sent one interval ago is answered or it is not,
        // and sending another before deciding would let a dead peer accumulate
        // probes forever.
        if let Some((at, seen)) = self.judge_at
            && now >= at
        {
            self.judge_at = None;
            if self.pongs.load(Ordering::Relaxed) == seen {
                return Due::Dead;
            }
        }

        if let Some(at) = self.next_ping
            && now >= at
        {
            self.next_ping = Some(now + self.interval);
            if !self.timeout.is_zero() {
                self.judge_at = Some((now + self.timeout, self.pongs.load(Ordering::Relaxed)));
            }
            // The payload is unused by anything — a peer must echo it back and
            // pahoa does not check which ping a pong answers, because with one
            // outstanding at a time there is only ever one it could answer.
            return Due::Ping(ws::frame::build(ws::frame::OpCode::Ping, false, b""));
        }

        Due::Nothing
    }
}

/// How long to let a closing room's last frames drain.
///
/// Long enough for a loopback or same-cluster write to land, short enough to be
/// invisible against a pod's termination grace period.
const CLOSE_LINGER: Duration = Duration::from_millis(250);

/// Everything every connection on one listener shares.
///
/// Cloned per connection, and cheap to clone: the acceptor and the router are
/// each an `Arc`, and the sender is a channel handle. Only `feed` differs
/// between the two listeners, and it is what makes the scoped port scoped.
#[derive(Clone)]
struct Port {
    actor: mpsc::Sender<ActorMsg>,
    config: NetConfig,
    tls: Option<tokio_rustls::TlsAcceptor>,
    router: crate::http::Router,
    feed: FeedPolicy,
}

/// Take connections until the task is aborted. One of these per listener.
fn accept_loop(
    listener: TcpListener,
    port: Port,
    next_id: Arc<AtomicU64>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok((stream, peer)) => {
                    let conn = ConnId(next_id.fetch_add(1, Ordering::Relaxed));
                    let port = port.clone();
                    tokio::spawn(async move {
                        if let Err(e) = serve_connection(stream, peer, conn, port).await {
                            tracing::debug!(%conn, error = %e, "connection ended");
                        }
                    });
                }
                Err(e) => {
                    tracing::warn!(error = %e, "accept failed");
                    // Back off briefly rather than spinning on a persistent
                    // error such as EMFILE.
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                }
            }
        }
    })
}

/// Decide the scheme, then hand off to [`run_session`].
///
/// One port serves both, which is why the first byte is peeked rather than
/// read: `peek` leaves it in the kernel's buffer, so whichever branch wins gets
/// an untouched stream and neither needs a prefix or chain wrapper.
async fn serve_connection(
    mut stream: TcpStream,
    peer: SocketAddr,
    conn: ConnId,
    port: Port,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let config = &port.config;

    // Latency matters more than packing for a chat-and-checks protocol. Set on
    // the socket before anything wraps it.
    stream.set_nodelay(true).ok();

    // 0x16 is the TLS handshake content type, and no HTTP method can begin with
    // it — every one of those is uppercase ASCII. One byte is therefore enough
    // to route, and asking for two would spin when only the first has arrived.
    let first = peek_first_byte(&stream, config.handshake_timeout).await?;
    let client_hello = first == Some(0x16);

    match (client_hello, &port.tls) {
        (true, Some(acceptor)) => {
            let mut stream = acceptor.accept(stream).await?;
            let Some(upgraded) = handshake(&mut stream, config, &port.router, peer).await? else {
                return Ok(());
            };
            // `TlsStream` has no `into_split`, so this pays for a `BiLock`. Only
            // TLS connections do; the plaintext path below keeps the cheaper
            // owned halves.
            let (read_half, write_half) = tokio::io::split(stream);
            run_session(read_half, write_half, upgraded, peer, conn, &port).await
        }
        // No certificate configured. Unchanged from before TLS existed: the
        // handshake_failure alert is what turns a `wss://`-first client's probe
        // into an immediate fallback rather than a 30-second hang.
        (true, None) => {
            let e = ws::accept::AcceptError::Tls;
            ws::accept::reject(&mut stream, &e).await;
            Err(e.into())
        }
        // Plaintext, with a certificate configured and no opt-in.
        (false, Some(_)) if !config.allow_plaintext => {
            refuse_plaintext(&mut stream, config, conn, peer).await;
            Err("plaintext refused: TLS is configured".into())
        }
        (false, _) => {
            let Some(upgraded) = handshake(&mut stream, config, &port.router, peer).await? else {
                return Ok(());
            };
            let (read_half, write_half) = stream.into_split();
            run_session(read_half, write_half, upgraded, peer, conn, &port).await
        }
    }
}

/// What a plaintext *HTTP* client is told when the room is serving TLS.
const UPGRADE_TO_TLS: &[u8] = b"HTTP/1.1 426 Upgrade Required\r\n\
    Upgrade: TLS/1.3, HTTP/1.1\r\n\
    Connection: close\r\n\
    Content-Length: 0\r\n\r\n";

/// Refuse a plaintext connection to a TLS room, differently depending on who is
/// asking.
///
/// **A WebSocket client is closed on without a reply, and that is deliberate
/// even though a `426` is the correct status.** Archipelago clients are given a
/// bare `host:port` and try `ws://` first — `CommonClient.py:857` prepends it
/// when the address carries no scheme. They recover through one specific
/// heuristic: `websockets` raises `InvalidMessage` when the reply is not
/// parseable HTTP, and `CommonClient.py:887-890` reads that as "probably
/// encrypted" and retries the same address as `wss://`. Against a room behind
/// an ordinary TLS terminator the plaintext attempt gets alert bytes, so the
/// retry fires and the player never learns any of this happened.
///
/// A well-formed `426` breaks exactly that. `websockets` parses it fine and
/// raises `InvalidStatusCode`, which is *not* the branch that retries — so the
/// standards-correct answer is the one that strands a client the reference's
/// accidental answer would have connected. Measured both ways rather than
/// reasoned about.
///
/// Anything that is not an upgrade — `curl`, a browser, a health check — still
/// gets the `426` with its `Upgrade` header, because for those the legible
/// answer is also the useful one and no fallback is riding on it.
async fn refuse_plaintext(
    stream: &mut TcpStream,
    config: &NetConfig,
    conn: ConnId,
    peer: SocketAddr,
) {
    let mut buf = Vec::with_capacity(1024);
    let upgrade = read_head(stream, config, &mut buf).await;

    if upgrade {
        // No reply at all: an unparseable response is what the client is
        // watching for, and closing is the cheapest way to produce one.
        tracing::debug!(
            %conn, %peer,
            "refused a plaintext WebSocket upgrade; closing so the client retries over TLS"
        );
        return;
    }

    let _ = stream.write_all(UPGRADE_TO_TLS).await;
    let _ = stream.flush().await;
    tracing::debug!(%conn, %peer, "refused a plaintext request; TLS is configured");
}

/// Read the request head far enough to tell an upgrade from ordinary HTTP.
///
/// Bounded by the same limits the real handshake uses, and any failure to read
/// or parse answers `false` — the `426` is the safer thing to send to something
/// that did not manage to ask a question.
async fn read_head(stream: &mut TcpStream, config: &NetConfig, buf: &mut Vec<u8>) -> bool {
    let deadline = tokio::time::Instant::now() + config.handshake_timeout;
    loop {
        if ws::handshake::headers_complete(buf) {
            break;
        }
        if buf.len() > config.max_header_bytes {
            return false;
        }
        let read = tokio::time::timeout_at(deadline, stream.read_buf(buf)).await;
        match read {
            Ok(Ok(0)) | Ok(Err(_)) | Err(_) => return false,
            Ok(Ok(_)) => {}
        }
    }
    ws::handshake::parse_request(buf, config.max_header_bytes)
        .is_ok_and(|request| request.is_upgrade())
}

/// Look at the first byte without consuming it.
///
/// `None` means the peer closed without sending anything.
async fn peek_first_byte(stream: &TcpStream, timeout: Duration) -> io::Result<Option<u8>> {
    let mut buf = [0u8; 1];
    let read = tokio::time::timeout(timeout, stream.peek(&mut buf))
        .await
        .map_err(|_| {
            io::Error::new(
                io::ErrorKind::TimedOut,
                "no first byte within the handshake timeout",
            )
        })??;
    Ok((read > 0).then_some(buf[0]))
}

/// Read the request and either upgrade, or answer it as HTTP.
///
/// `Ok(None)` means the request was served over HTTP and the connection is
/// finished — which is the ordinary outcome for a readiness probe or an admin
/// call, not an error.
async fn handshake<S>(
    stream: &mut S,
    config: &NetConfig,
    router: &crate::http::Router,
    peer: SocketAddr,
) -> Result<Option<ws::accept::Upgraded>, ws::accept::AcceptError>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    match ws::accept::accept(stream, &config.accept_config()).await {
        Ok(ws::accept::Accepted::WebSocket(upgraded)) => Ok(Some(upgraded)),
        Ok(ws::accept::Accepted::Http(exchange)) => {
            let response = router.route(&exchange, peer.ip()).await;
            let _ = stream.write_all(&response.render()).await;
            let _ = stream.flush().await;
            Ok(None)
        }
        Err(e) => {
            // A broken upgrade, or a request too large to read, gets a status
            // rather than a silently dropped socket.
            ws::accept::reject(stream, &e).await;
            Err(e)
        }
    }
}

/// Everything after the scheme is settled, over whichever stream won.
///
/// Generic so the plaintext path can keep `TcpStream`'s owned halves and the
/// TLS path can use `tokio::io::split`; nothing below here is socket-typed.
async fn run_session<R, W>(
    mut read_half: R,
    mut write_half: W,
    upgraded: ws::accept::Upgraded,
    peer: SocketAddr,
    conn: ConnId,
    port: &Port,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>>
where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let Port { actor, config, .. } = port;
    let feed = port.feed;

    let deflate = upgraded.deflate;
    tracing::debug!(%conn, %peer, deflate, "upgraded");

    // The real bound is the byte budget the shard checks before queuing. This
    // depth exists only so the channel is never the *tighter* limit: sized at
    // the message count the byte budget would already have refused, assuming a
    // 64-byte floor per message. Getting this wrong reintroduces exactly the
    // bound-by-message-count behavior the byte budget replaced — a burst of
    // small chat frames would hit the channel long before the budget and drop
    // connections that were nowhere near their share.
    //
    // tokio's mpsc does not preallocate, so a generous capacity costs nothing
    // until it is used.
    let depth = (config.per_connection_budget_bytes / 64).max(256);
    let (out_tx, mut out_rx) = mpsc::channel::<Outbound>(depth);
    // Deliberately separate from `out_tx`, and deliberately tiny. A connection
    // is dropped for lagging precisely when `out_tx` is full, so a close
    // travelling on `out_tx` cannot reach the socket in the case that matters.
    // See `shard::CloseSignal`.
    let (close_tx, mut close_rx) = mpsc::channel::<&'static str>(1);
    // Bumped by the reader, read by the writer's keepalive. A counter rather
    // than a timestamp: the only question is whether anything answered since
    // the last ping went out.
    let pongs = Arc::new(AtomicU64::new(0));
    let conn_budget: crate::budget::ConnHandle = Arc::new(crate::budget::ConnBudget::default());

    if actor
        .send(ActorMsg::Connected {
            feed,
            conn,
            tx: out_tx.clone(),
            close: close_tx.clone(),
            deflate,
            budget: Arc::clone(&conn_budget),
        })
        .await
        .is_err()
    {
        return Ok(());
    }

    // Writer: owns the socket's write half for this connection's lifetime, and
    // writes pre-built frames verbatim. It does no framing and no compression —
    // that already happened once, in the shard, for every recipient at once.
    let mut writer = {
        let conn_budget = Arc::clone(&conn_budget);
        let pongs = Arc::clone(&pongs);
        let mut keepalive = Keepalive::new(config.ping_interval, config.ping_timeout, pongs);
        tokio::spawn(async move {
            loop {
                // `biased`, so a forced close is taken ahead of queued frames.
                // That ordering is right: this arm only fires when the ordered
                // close could not be queued, which means those frames were not
                // reaching the client either.
                let out = tokio::select! {
                    biased;
                    Some(reason) = close_rx.recv() => {
                        tracing::debug!(%conn, reason, "closing out of band");
                        break;
                    }
                    () = keepalive.wait() => {
                        // Written straight to the socket rather than queued: a
                        // liveness probe that waits behind a backlog measures
                        // the backlog, not the peer.
                        match keepalive.due() {
                            Due::Dead => {
                                tracing::info!(%conn, "no pong within the keepalive timeout");
                                break;
                            }
                            Due::Ping(frame) => {
                                if write_half.write_all(&frame).await.is_err() {
                                    break;
                                }
                                continue;
                            }
                            Due::Nothing => continue,
                        }
                    }
                    frame = out_rx.recv() => match frame {
                        Some(frame) => frame,
                        None => break,
                    },
                };
                let result = match out {
                    Outbound::Frame(bytes) => {
                        let size = bytes.len();
                        // The write races the forced close, because a peer that
                        // has stopped reading will fill its receive window and
                        // leave `write_all` pending forever. Without this the
                        // signal would be delivered to a task that never
                        // reaches a `select!` again, which is precisely the
                        // stuck case it exists to break.
                        let written = tokio::select! {
                            biased;
                            Some(reason) = close_rx.recv() => {
                                tracing::debug!(%conn, reason, "closing mid-write");
                                crate::budget::Budget::release(&conn_budget, size);
                                break;
                            }
                            written = write_half.write_all(&bytes) => written,
                        };
                        // Released once the bytes are the kernel's problem
                        // rather than ours, which is what makes the budget a
                        // measure of what pahoa is actually holding.
                        crate::budget::Budget::release(&conn_budget, size);
                        written
                    }
                    Outbound::Close(reason) => {
                        tracing::debug!(%conn, reason, "closing");
                        let _ = write_half
                            .write_all(&ws::frame::close(CLOSE_GOING_AWAY, reason))
                            .await;
                        let _ = write_half.flush().await;
                        break;
                    }
                };
                if result.is_err() {
                    break;
                }
            }
        })
    };

    // Reader: framing, inflation and JSON parsing all happen here, on this
    // connection's own task, so none of it lands on the actor.
    let mut session = ws::message::Session::new(upgraded.inflater, config.max_message_bytes);
    let mut buf = upgraded.leftover;
    let outcome = 'read: loop {
        // Drain whatever is already buffered before asking for more; a single
        // read commonly carries several frames.
        loop {
            match ws::frame::decode(&mut buf, config.max_frame_bytes) {
                Ok(Some(frame)) => match session.handle(frame) {
                    Ok(Some(event)) => {
                        if let Some(reason) =
                            handle_event(event, conn, actor, &out_tx, &pongs).await
                        {
                            break 'read reason;
                        }
                    }
                    Ok(None) => {}
                    Err(e) => break 'read format!("protocol error: {e}"),
                },
                Ok(None) => break,
                Err(e) => break 'read format!("bad frame: {e}"),
            }
        }

        // **The writer finishing ends the connection.** Without this the reader
        // waits on a peer that has already been told to go away — or, worse,
        // one that was never able to hear it — and the socket stays open with
        // the room no longer tracking it. That is the half-open state a client
        // cannot detect: it believes it is playing, and nothing it sends is
        // heard. Both halves must drop for the socket to close, so the reader
        // has to learn about a close the writer decided on.
        tokio::select! {
            biased;
            _ = &mut writer => break "closed by the server".to_string(),
            read = read_half.read_buf(&mut buf) => match read {
                Ok(0) => break "peer closed".to_string(),
                Ok(_) => {}
                Err(e) => break format!("read failed: {e}"),
            },
        }
    };
    tracing::debug!(%conn, %peer, outcome, "connection ended");

    let _ = actor.send(ActorMsg::Disconnected { conn }).await;
    writer.abort();
    Ok(())
}

/// Act on one inbound message. Returns `Some(reason)` when the connection is
/// finished.
async fn handle_event(
    event: ws::message::Event,
    conn: ConnId,
    actor: &mpsc::Sender<ActorMsg>,
    out: &mpsc::Sender<Outbound>,
    pongs: &AtomicU64,
) -> Option<String> {
    use ws::message::Event;
    match event {
        Event::Text(text) => match decode(&text) {
            Ok(packets) => {
                if actor
                    .send(ActorMsg::Packets { conn, packets })
                    .await
                    .is_err()
                {
                    return Some("room stopped".into());
                }
                None
            }
            Err(e) => {
                // The reference server drops the socket rather than answering
                // `InvalidPacket`, and the room owns that decision.
                let _ = actor
                    .send(ActorMsg::DecodeFailed {
                        conn,
                        detail: e.to_string(),
                    })
                    .await;
                Some("undecodable frame".into())
            }
        },
        // Archipelago is a text protocol; binary frames are not part of it, but
        // the layer below still had to reassemble them correctly.
        Event::Binary(_) => None,
        Event::Ping(payload) => {
            let pong = ws::frame::build(ws::frame::OpCode::Pong, false, &payload);
            let _ = out.try_send(Outbound::Frame(pong));
            None
        }
        Event::Pong(_) => {
            // The peer is alive. Which ping this answers does not matter: only
            // one is ever outstanding, so any pong clears it.
            pongs.fetch_add(1, Ordering::Relaxed);
            None
        }
        Event::Close(frame) => {
            // Echo the peer's code back, as RFC 6455 §5.5.1 requires.
            let (code, reason) = frame.unwrap_or((CLOSE_NORMAL, String::new()));
            let echo = ws::frame::close(code, &reason);
            let _ = out.try_send(Outbound::Frame(echo));
            Some(format!("peer closed ({code})"))
        }
    }
}

const CLOSE_NORMAL: u16 = 1000;
const CLOSE_GOING_AWAY: u16 = 1001;

/// Build a multi-threaded runtime sized for the container, not the host.
pub fn build_runtime(config: &NetConfig) -> io::Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(config.worker_threads_resolved())
        // The only blocking work is saving, and it coalesces to one at a time.
        .max_blocking_threads(4)
        .enable_all()
        .build()
}

/// Convenience for embedding a room in an existing runtime.
pub async fn serve(room: Room, config: NetConfig) -> io::Result<Arc<Server>> {
    Ok(Arc::new(Server::start(room, config).await?))
}
