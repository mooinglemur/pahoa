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
use pahoa_room::{ConnId, Room};
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
    actor_tx: mpsc::Sender<ActorMsg>,
    /// Fires when the actor loop has returned, final save included. Taken by
    /// the first `shutdown` caller; a second call has nothing left to wait for.
    stopped: tokio::sync::Mutex<Option<tokio::sync::oneshot::Receiver<()>>>,
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
        let router = crate::http::Router::new(room.multidata_arc(), actor_tx.clone());

        let (stopped_tx, stopped_rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            actor::run_with_saves(&mut room, shards, actor_rx, saves).await;
            let _ = stopped_tx.send(());
        });

        let accept_tx = actor_tx.clone();
        let cfg = config.clone();
        tokio::spawn(async move {
            let next_id = AtomicU64::new(1);
            loop {
                match listener.accept().await {
                    Ok((stream, peer)) => {
                        let conn = ConnId(next_id.fetch_add(1, Ordering::Relaxed));
                        let tx = accept_tx.clone();
                        let cfg = cfg.clone();
                        // Both cheap: a `TlsAcceptor` is an `Arc<ServerConfig>`
                        // and a `Router` is an `Arc` of its own.
                        let tls = tls.clone();
                        let router = router.clone();
                        tokio::spawn(async move {
                            if let Err(e) =
                                serve_connection(stream, peer, conn, tx, cfg, tls, router).await
                            {
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
        });

        Ok(Self {
            local_addr,
            actor_tx,
            stopped: tokio::sync::Mutex::new(Some(stopped_rx)),
        })
    }

    /// Stop the room and wait for it to finish, including its final save.
    ///
    /// Waiting matters: without it the process can exit while the flush is
    /// still on a blocking thread, which throws away the very state the flush
    /// exists to keep. The flush has its own timeout, so this cannot hang
    /// indefinitely on a stuck filesystem.
    pub async fn shutdown(&self) {
        let _ = self.actor_tx.send(ActorMsg::Shutdown).await;
        if let Some(stopped) = self.stopped.lock().await.take() {
            let _ = stopped.await;
        }
    }
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
    actor: mpsc::Sender<ActorMsg>,
    config: NetConfig,
    tls: Option<tokio_rustls::TlsAcceptor>,
    router: crate::http::Router,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Latency matters more than packing for a chat-and-checks protocol. Set on
    // the socket before anything wraps it.
    stream.set_nodelay(true).ok();

    // 0x16 is the TLS handshake content type, and no HTTP method can begin with
    // it — every one of those is uppercase ASCII. One byte is therefore enough
    // to route, and asking for two would spin when only the first has arrived.
    let first = peek_first_byte(&stream, config.handshake_timeout).await?;
    let client_hello = first == Some(0x16);

    match (client_hello, &tls) {
        (true, Some(acceptor)) => {
            let mut stream = acceptor.accept(stream).await?;
            let Some(upgraded) = handshake(&mut stream, &config, &router).await? else {
                return Ok(());
            };
            // `TlsStream` has no `into_split`, so this pays for a `BiLock`. Only
            // TLS connections do; the plaintext path below keeps the cheaper
            // owned halves.
            let (read_half, write_half) = tokio::io::split(stream);
            run_session(read_half, write_half, upgraded, peer, conn, actor, config).await
        }
        // No certificate configured. Unchanged from before TLS existed: the
        // handshake_failure alert is what turns a `wss://`-first client's probe
        // into an immediate fallback rather than a 30-second hang.
        (true, None) => {
            let e = ws::accept::AcceptError::Tls;
            ws::accept::reject(&mut stream, &e).await;
            Err(e.into())
        }
        // Plaintext, with a certificate configured and no opt-in. RFC 2817's
        // status for exactly this, so the refusal is legible to a person with
        // curl rather than a bare disconnect.
        (false, Some(_)) if !config.allow_plaintext => {
            let _ = stream.write_all(UPGRADE_TO_TLS).await;
            let _ = stream.flush().await;
            tracing::debug!(%conn, %peer, "refused a plaintext connection; TLS is configured");
            Err("plaintext refused: TLS is configured".into())
        }
        (false, _) => {
            let Some(upgraded) = handshake(&mut stream, &config, &router).await? else {
                return Ok(());
            };
            let (read_half, write_half) = stream.into_split();
            run_session(read_half, write_half, upgraded, peer, conn, actor, config).await
        }
    }
}

/// What a plaintext client is told when the room is serving TLS.
const UPGRADE_TO_TLS: &[u8] = b"HTTP/1.1 426 Upgrade Required\r\n\
    Upgrade: TLS/1.3, HTTP/1.1\r\n\
    Connection: close\r\n\
    Content-Length: 0\r\n\r\n";

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
) -> Result<Option<ws::accept::Upgraded>, ws::accept::AcceptError>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    match ws::accept::accept(stream, &config.accept_config()).await {
        Ok(ws::accept::Accepted::WebSocket(upgraded)) => Ok(Some(upgraded)),
        Ok(ws::accept::Accepted::Http(exchange)) => {
            let response = router.route(&exchange).await;
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
    actor: mpsc::Sender<ActorMsg>,
    config: NetConfig,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>>
where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin + Send + 'static,
{
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
    let conn_budget: crate::budget::ConnHandle = Arc::new(crate::budget::ConnBudget::default());

    if actor
        .send(ActorMsg::Connected {
            conn,
            tx: out_tx.clone(),
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
    let writer = {
        let conn_budget = Arc::clone(&conn_budget);
        tokio::spawn(async move {
            while let Some(out) = out_rx.recv().await {
                let result = match out {
                    Outbound::Frame(bytes) => {
                        let size = bytes.len();
                        let written = write_half.write_all(&bytes).await;
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
                        if let Some(reason) = handle_event(event, conn, &actor, &out_tx).await {
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

        match read_half.read_buf(&mut buf).await {
            Ok(0) => break "peer closed".to_string(),
            Ok(_) => {}
            Err(e) => break format!("read failed: {e}"),
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
        Event::Pong(_) => None,
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
