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
        let listener = TcpListener::bind((config.bind.as_str(), config.port)).await?;
        let local_addr = listener.local_addr()?;

        let shards = Shards::spawn(config.shards_resolved(), 4096, config.compression_level);
        let (actor_tx, actor_rx) = mpsc::channel(8192);

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
                        tokio::spawn(async move {
                            if let Err(e) = serve_connection(stream, peer, conn, tx, cfg).await {
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

async fn serve_connection(
    mut stream: TcpStream,
    peer: SocketAddr,
    conn: ConnId,
    actor: mpsc::Sender<ActorMsg>,
    config: NetConfig,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Latency matters more than packing for a chat-and-checks protocol.
    stream.set_nodelay(true).ok();

    let upgraded = match ws::accept::accept(&mut stream, &config.accept_config()).await {
        Ok(u) => u,
        Err(e) => {
            // A health check or a stray browser gets an HTTP status rather than
            // a silently dropped socket.
            ws::accept::reject(&mut stream, &e).await;
            return Err(e.into());
        }
    };
    let deflate = upgraded.deflate;
    tracing::debug!(%conn, %peer, deflate, "upgraded");

    let (mut read_half, mut write_half) = stream.into_split();

    // Bounded by bytes-worth-of-messages rather than unbounded: a client that
    // cannot keep up must not be able to grow the server's memory without limit.
    let depth = (config.per_connection_budget_bytes / 4096).max(8);
    let (out_tx, mut out_rx) = mpsc::channel::<Outbound>(depth);

    if actor
        .send(ActorMsg::Connected {
            conn,
            tx: out_tx.clone(),
            deflate,
        })
        .await
        .is_err()
    {
        return Ok(());
    }

    // Writer: owns the socket's write half for this connection's lifetime, and
    // writes pre-built frames verbatim. It does no framing and no compression —
    // that already happened once, in the shard, for every recipient at once.
    let writer = tokio::spawn(async move {
        while let Some(out) = out_rx.recv().await {
            let result = match out {
                Outbound::Frame(bytes) => write_half.write_all(&bytes).await,
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
    });

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
