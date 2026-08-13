//! A bare echo server on pahoa's WebSocket layer, for the Autobahn suite.
//!
//! pahoa itself speaks Archipelago, not echo, so conformance cannot be measured
//! against it directly. This exposes the same [`pahoa_net::ws`] code — framing,
//! permessage-deflate, fragmentation, the control-frame rules — with an echo on
//! top, which is what `wstest` knows how to grade.
//!
//! ```sh
//! cargo run --release -p pahoa-net --example ws-echo -- 9001
//! docker run --rm --network host -v "$PWD/tools/autobahn:/config" \
//!     -v "$PWD/target/autobahn:/reports" \
//!     crossbario/autobahn-testsuite \
//!     wstest -m fuzzingclient -s /config/fuzzingclient.json
//! ```

use pahoa_net::ws;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let port: u16 = std::env::args()
        .nth(1)
        .and_then(|a| a.parse().ok())
        .unwrap_or(9001);
    let listener = TcpListener::bind(("127.0.0.1", port)).await?;
    eprintln!("ws-echo listening on {}", listener.local_addr()?);

    loop {
        let (stream, _) = listener.accept().await?;
        tokio::spawn(async move {
            if let Err(e) = echo(stream).await {
                eprintln!("connection ended: {e}");
            }
        });
    }
}

async fn echo(mut stream: TcpStream) -> Result<(), Box<dyn std::error::Error>> {
    stream.set_nodelay(true).ok();

    // Autobahn's limit cases go well past what Archipelago ever sends, so the
    // caps here are deliberately generous — this is measuring conformance, not
    // the production budget.
    let config = ws::accept::AcceptConfig {
        deflate: ws::handshake::DeflateConfig::default(),
        max_headers: 16 * 1024,
        timeout: Duration::from_secs(10),
        max_message: 64 * 1024 * 1024,
    };

    let upgraded = match ws::accept::accept(&mut stream, &config).await {
        Ok(u) => u,
        Err(e) => {
            ws::accept::reject(&mut stream, &e).await;
            return Err(e.into());
        }
    };

    // The *negotiated* window, not the default. A client may cap ours below 11,
    // and compressing with a larger window than we promised emits
    // back-references it cannot resolve — which only shows up once a payload is
    // big enough to reach past the client's window.
    let mut deflater = upgraded
        .deflate
        .map(|bits| ws::deflate::Deflater::new(6, bits));
    let mut session = ws::message::Session::new(upgraded.inflater, config.max_message);
    let mut buf = upgraded.leftover;

    loop {
        loop {
            let frame = match ws::frame::decode(&mut buf, config.max_message) {
                Ok(Some(f)) => f,
                Ok(None) => break,
                Err(e) => {
                    // A framing violation is a 1002 close, which is what the
                    // suite checks for on its malformed-input cases.
                    let _ = stream
                        .write_all(&ws::frame::close(1002, &e.to_string()))
                        .await;
                    return Ok(());
                }
            };

            let event = match session.handle(frame) {
                Ok(Some(e)) => e,
                Ok(None) => continue,
                Err(e) => {
                    // 1007 is specifically "data inconsistent with the message
                    // type", which is what an invalid-UTF-8 payload is; every
                    // other rule violation is a plain protocol error.
                    let code = match e {
                        ws::message::ProtocolError::NotUtf8 => 1007,
                        _ => 1002,
                    };
                    let _ = stream.write_all(&ws::frame::close(code, "")).await;
                    return Ok(());
                }
            };

            let reply = match event {
                ws::message::Event::Text(text) => {
                    frame_for(ws::frame::OpCode::Text, text.as_bytes(), &mut deflater)
                }
                ws::message::Event::Binary(payload) => {
                    frame_for(ws::frame::OpCode::Binary, &payload, &mut deflater)
                }
                ws::message::Event::Ping(payload) => {
                    ws::frame::build(ws::frame::OpCode::Pong, false, &payload)
                }
                ws::message::Event::Pong(_) => continue,
                ws::message::Event::Close(frame) => {
                    let (code, reason) = frame.unwrap_or((1000, String::new()));
                    let _ = stream.write_all(&ws::frame::close(code, &reason)).await;
                    return Ok(());
                }
            };
            stream.write_all(&reply).await?;
        }

        if stream.read_buf(&mut buf).await? == 0 {
            return Ok(());
        }
    }
}

/// Echo a payload, compressing it when the connection negotiated deflate.
fn frame_for(
    opcode: ws::frame::OpCode,
    payload: &[u8],
    deflater: &mut Option<ws::deflate::Deflater>,
) -> bytes::Bytes {
    let Some(deflater) = deflater else {
        return ws::frame::build(opcode, false, payload);
    };
    let compressed = deflater.compress(payload);
    if compressed.len() >= payload.len() {
        // Legal and simply smaller: RSV1 is per-message.
        return ws::frame::build(opcode, false, payload);
    }
    ws::frame::build(opcode, true, &compressed)
}
