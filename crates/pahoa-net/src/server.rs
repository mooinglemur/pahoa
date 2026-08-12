//! Listener and per-connection tasks.
//!
//! Each connection gets two tasks: a reader that parses inbound frames and a
//! writer that owns the socket's write half. Parsing, and later TLS and
//! compression, therefore happen per-connection on worker threads rather than
//! on the single task that owns room state.

use crate::actor::{self, ActorMsg};
use crate::config::NetConfig;
use crate::shard::{Outbound, Shards};
use futures_util::{SinkExt, StreamExt};
use pahoa_proto::decode;
use pahoa_room::{ConnId, Room};
use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;

/// A running server.
pub struct Server {
    pub local_addr: SocketAddr,
    actor_tx: mpsc::Sender<ActorMsg>,
}

impl Server {
    /// Bind and start serving. Returns once the listener is up, so tests can
    /// connect without racing.
    pub async fn start(room: Room, config: NetConfig) -> io::Result<Self> {
        let listener = TcpListener::bind((config.bind.as_str(), config.port)).await?;
        let local_addr = listener.local_addr()?;

        let shards = Shards::spawn(config.shards_resolved(), 4096);
        let (actor_tx, actor_rx) = mpsc::channel(8192);

        tokio::spawn(actor::run(room, shards, actor_rx));

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
        })
    }

    pub async fn shutdown(&self) {
        let _ = self.actor_tx.send(ActorMsg::Shutdown).await;
    }
}

async fn serve_connection(
    stream: TcpStream,
    peer: SocketAddr,
    conn: ConnId,
    actor: mpsc::Sender<ActorMsg>,
    config: NetConfig,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Latency matters more than packing for a chat-and-checks protocol.
    stream.set_nodelay(true).ok();

    let ws_config = WebSocketConfig::default()
        // Matches the reference server's `max_size=2**20`.
        .max_message_size(Some(config.max_frame_bytes))
        .max_frame_size(Some(config.max_frame_bytes));

    let ws = tokio_tungstenite::accept_async_with_config(stream, Some(ws_config)).await?;
    let (mut sink, mut source) = ws.split();

    // Bounded by bytes-worth-of-messages rather than unbounded: a client that
    // cannot keep up must not be able to grow the server's memory without limit.
    let depth = (config.per_connection_budget_bytes / 4096).max(8);
    let (out_tx, mut out_rx) = mpsc::channel::<Outbound>(depth);

    if actor
        .send(ActorMsg::Connected { conn, tx: out_tx })
        .await
        .is_err()
    {
        return Ok(());
    }

    // Writer: owns the socket's write half for this connection's lifetime.
    let writer = tokio::spawn(async move {
        while let Some(out) = out_rx.recv().await {
            let result = match out {
                Outbound::Frame(bytes) => {
                    match std::str::from_utf8(&bytes) {
                        Ok(text) => sink.send(Message::text(text.to_owned())).await,
                        // encode() only ever produces UTF-8; treat a failure as
                        // fatal for this connection rather than silently skipping.
                        Err(_) => break,
                    }
                }
                Outbound::Close(reason) => {
                    tracing::debug!(%conn, reason, "closing");
                    let _ = sink.close().await;
                    break;
                }
            };
            if result.is_err() {
                break;
            }
        }
    });

    // Reader: parses inbound frames here, off the actor.
    while let Some(message) = source.next().await {
        let message = match message {
            Ok(m) => m,
            Err(e) => {
                tracing::debug!(%conn, %peer, error = %e, "read failed");
                break;
            }
        };

        let text = match message {
            Message::Text(t) => t,
            Message::Binary(_) => {
                // Archipelago is a text protocol; binary frames are not part of it.
                continue;
            }
            Message::Close(_) => break,
            // tungstenite answers pings itself.
            Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => continue,
        };

        match decode(text.as_str()) {
            Ok(packets) => {
                if actor
                    .send(ActorMsg::Packets { conn, packets })
                    .await
                    .is_err()
                {
                    break;
                }
            }
            Err(e) => {
                let _ = actor
                    .send(ActorMsg::DecodeFailed {
                        conn,
                        detail: e.to_string(),
                    })
                    .await;
                break;
            }
        }
    }

    let _ = actor.send(ActorMsg::Disconnected { conn }).await;
    writer.abort();
    Ok(())
}

/// Build a multi-threaded runtime sized for the container, not the host.
pub fn build_runtime(config: &NetConfig) -> io::Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(config.worker_threads_resolved())
        // The only blocking work is saving, which arrives at M7.
        .max_blocking_threads(4)
        .enable_all()
        .build()
}

/// Convenience for embedding a room in an existing runtime.
pub async fn serve(room: Room, config: NetConfig) -> io::Result<Arc<Server>> {
    Ok(Arc::new(Server::start(room, config).await?))
}
