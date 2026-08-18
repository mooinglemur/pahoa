//! Rendering an answer.
//!
//! Every response closes the connection. That is what the WebSocket path's
//! error responses already do, and it keeps this surface free of a keep-alive
//! state machine for clients — curl, a kubelet probe, an orchestrator — that
//! send one request and read one answer.

/// A rendered answer, built before anything is written.
///
/// A value rather than a series of writes, so a route can be tested by calling
/// it rather than by standing up a socket.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response {
    pub status: u16,
    reason: &'static str,
    content_type: &'static str,
    /// Extra headers, in order. Rare enough not to deserve a map.
    extra: Vec<(&'static str, String)>,
    pub body: Vec<u8>,
}

impl Response {
    pub fn status(status: u16, reason: &'static str) -> Self {
        Self {
            status,
            reason,
            content_type: "text/plain; charset=utf-8",
            extra: Vec::new(),
            body: Vec::new(),
        }
    }

    pub fn text(status: u16, body: &str) -> Self {
        Self {
            body: body.as_bytes().to_vec(),
            ..Self::status(status, reason_for(status))
        }
    }

    pub fn json(status: u16, value: &serde_json::Value) -> Self {
        Self {
            content_type: "application/json",
            // Serializing a value we built ourselves cannot fail, but a panic
            // in a request handler is a worse answer than a 500.
            body: serde_json::to_vec(value).unwrap_or_else(|_| b"{}".to_vec()),
            ..Self::status(status, reason_for(status))
        }
    }

    /// JSON that is already rendered.
    ///
    /// For documents built once and served many times, where re-serializing per
    /// request would be the expensive part.
    pub fn json_bytes(status: u16, body: Vec<u8>) -> Self {
        Self {
            content_type: "application/json",
            body,
            ..Self::status(status, reason_for(status))
        }
    }

    /// Prometheus text exposition, which has its own content type.
    pub fn prometheus(body: String) -> Self {
        Self {
            content_type: "text/plain; version=0.0.4; charset=utf-8",
            body: body.into_bytes(),
            ..Self::status(200, "OK")
        }
    }

    pub fn not_found() -> Self {
        Self::status(404, "Not Found")
    }

    pub fn with_header(mut self, name: &'static str, value: impl Into<String>) -> Self {
        self.extra.push((name, value.into()));
        self
    }

    pub fn render(&self) -> Vec<u8> {
        let mut head = format!(
            "HTTP/1.1 {} {}\r\nConnection: close\r\nContent-Type: {}\r\nContent-Length: {}\r\n",
            self.status,
            self.reason,
            self.content_type,
            self.body.len()
        );
        for (name, value) in &self.extra {
            head.push_str(&format!("{name}: {value}\r\n"));
        }
        head.push_str("\r\n");

        let mut out = head.into_bytes();
        out.extend_from_slice(&self.body);
        out
    }
}

/// Only the statuses this surface actually returns.
fn reason_for(status: u16) -> &'static str {
    match status {
        200 => "OK",
        202 => "Accepted",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        405 => "Method Not Allowed",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        _ => "",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rendered(r: Response) -> String {
        String::from_utf8(r.render()).unwrap()
    }

    #[test]
    fn a_response_carries_its_length_and_closes() {
        let out = rendered(Response::text(200, "ok\n"));
        assert!(out.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(out.contains("Content-Length: 3\r\n"));
        assert!(out.contains("Connection: close\r\n"));
        assert!(out.ends_with("\r\n\r\nok\n"));
    }

    #[test]
    fn json_says_so() {
        let out = rendered(Response::json(200, &serde_json::json!({"a": 1})));
        assert!(out.contains("Content-Type: application/json\r\n"));
        assert!(out.ends_with("{\"a\":1}"));
    }

    #[test]
    fn an_empty_body_still_carries_a_zero_length() {
        // Without it a client waits for a body that never arrives.
        let out = rendered(Response::not_found());
        assert!(out.contains("Content-Length: 0\r\n"), "{out:?}");
    }

    #[test]
    fn extra_headers_are_kept_in_order() {
        let out = rendered(
            Response::status(401, "Unauthorized")
                .with_header("WWW-Authenticate", "Bearer")
                .with_header("Retry-After", "60"),
        );
        let auth = out.find("WWW-Authenticate: Bearer").unwrap();
        let retry = out.find("Retry-After: 60").unwrap();
        assert!(auth < retry);
    }
}
