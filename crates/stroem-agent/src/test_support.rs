//! Test helpers: a one-shot HTTP listener that records the request a
//! provider client sends.

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::task::JoinHandle;

/// One request as the provider sent it.
pub(crate) struct Captured {
    pub request_line: String,
    /// Header lines, lowercased.
    pub headers: String,
    pub body: serde_json::Value,
}

/// Bind a local listener that accepts one request, answers it `400 Bad
/// Request` (so the provider call fails) and yields what it received.
/// Returns the listener's `http://host:port` base URL.
pub(crate) async fn capture_one_request() -> (String, JoinHandle<Captured>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base_url = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut raw = Vec::new();
        let mut buf = [0u8; 8192];
        loop {
            let n = socket.read(&mut buf).await.unwrap();
            raw.extend_from_slice(&buf[..n]);
            let text = String::from_utf8_lossy(&raw).to_string();
            if let Some(split) = text.find("\r\n\r\n") {
                let head = text[..split].to_string();
                let len = head
                    .lines()
                    .find_map(|line| {
                        let (key, value) = line.split_once(':')?;
                        key.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().unwrap())
                    })
                    .unwrap_or(0);
                if raw.len() >= split + 4 + len {
                    let body = &raw[split + 4..split + 4 + len];
                    socket
                        .write_all(b"HTTP/1.1 400 Bad Request\r\ncontent-length: 0\r\n\r\n")
                        .await
                        .unwrap();
                    let (request_line, headers) = head.split_once("\r\n").unwrap();
                    return Captured {
                        request_line: request_line.to_string(),
                        headers: headers.to_ascii_lowercase(),
                        body: serde_json::from_slice(body).unwrap(),
                    };
                }
            }
            assert!(n > 0, "client closed before sending a full request");
        }
    });
    (base_url, server)
}
