use base64::{engine::general_purpose::STANDARD, Engine};
use futures_util::{SinkExt, StreamExt};
use std::net::SocketAddr;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::tungstenite::protocol::Message;
use tracing::{error, info, warn};

type BoxError = Box<dyn std::error::Error + Send + Sync>;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            std::env::var("RUST_LOG").unwrap_or_else(|_| "info".to_string()),
        )
        .init();

    let port: u16 = std::env::var("PORT")
        .unwrap_or_else(|_| "8080".to_string())
        .parse()
        .expect("PORT must be a number");

    let token = std::env::var("PROXY_TOKEN").unwrap_or_else(|_| {
        warn!("PROXY_TOKEN not set — proxy is open to anyone!");
        String::new()
    });

    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    let listener = TcpListener::bind(addr).await.expect("Failed to bind");
    info!("Proxy listening on {addr}");

    loop {
        let (stream, peer) = match listener.accept().await {
            Ok(v) => v,
            Err(e) => { error!("Accept error: {e}"); continue; }
        };
        let token = token.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_connection(stream, peer, &token).await {
                let msg = e.to_string();
                if !msg.contains("connection reset")
                    && !msg.contains("broken pipe")
                    && !msg.contains("Connection reset")
                {
                    error!("Error from {peer}: {e}");
                }
            }
        });
    }
}

/// Peek at the first HTTP request line to decide routing.
/// /health → plain HTTP response
/// /tunnel → WebSocket accept directly on TCP stream
async fn handle_connection(
    mut stream: TcpStream,
    peer: SocketAddr,
    token: &str,
) -> Result<(), BoxError> {
    // Peek at the request without consuming bytes
    let mut buf = vec![0u8; 4096];
    let n = stream.peek(&mut buf).await?;
    let head = std::str::from_utf8(&buf[..n]).unwrap_or("");

    // Parse request line: "GET /path HTTP/1.1"
    let request_line = head.lines().next().unwrap_or("");
    let path = request_line
        .split_whitespace()
        .nth(1)
        .unwrap_or("");

    if path == "/health" {
        // Consume the request bytes, send plain HTTP response
        let _ = stream.read(&mut buf).await;
        let body = "ok";
        let resp = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        stream.write_all(resp.as_bytes()).await?;
        return Ok(());
    }

    if path.starts_with("/tunnel") {
        return handle_tunnel(stream, peer, token, head).await;
    }

    // Everything else → 404
    let _ = stream.read(&mut buf).await;
    let body = "not found";
    let resp = format!(
        "HTTP/1.1 404 Not Found\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    stream.write_all(resp.as_bytes()).await?;
    Ok(())
}

/// Handle /tunnel?target=host:port — auth check, then WebSocket accept
async fn handle_tunnel(
    mut stream: TcpStream,
    peer: SocketAddr,
    token: &str,
    head: &str,
) -> Result<(), BoxError> {
    // Extract target from query string in the request line
    let request_line = head.lines().next().unwrap_or("");
    let path_and_query = request_line.split_whitespace().nth(1).unwrap_or("");
    let target = path_and_query
        .split('?')
        .nth(1)
        .and_then(|q| {
            q.split('&')
                .find_map(|p| p.strip_prefix("target="))
        })
        .unwrap_or("")
        .to_string();

    // Auth check: look for Authorization header in raw request
    if !token.is_empty() && !check_auth_raw(head, token) {
        warn!("Unauthorized tunnel request from {peer}");
        // Consume peeked bytes before sending error
        let mut discard = vec![0u8; 4096];
        let _ = stream.read(&mut discard).await;
        return send_error(stream, 401, "unauthorized").await;
    }

    if target.is_empty() {
        let mut discard = vec![0u8; 4096];
        let _ = stream.read(&mut discard).await;
        return send_error(stream, 400, "missing target query param").await;
    }

    info!("Tunnel request for {target} from {peer}");

    // accept_async reads the request bytes itself (they're still in the buffer from peek)
    let ws = tokio_tungstenite::accept_async(stream).await?;

    info!("WS accepted for {target} from {peer}");

    // Connect to the actual target
    let tcp = TcpStream::connect(&target).await?;
    let (mut tcp_read, mut tcp_write) = tokio::io::split(tcp);
    let (mut ws_sink, mut ws_stream) = ws.split();

    info!("Tunnel established to {target}");

    ws_relay(&mut ws_stream, &mut ws_sink, &mut tcp_read, &mut tcp_write).await;
    Ok(())
}

/// Bidirectional relay: WebSocket binary frames ↔ TCP bytes
async fn ws_relay(
    ws_stream: &mut (impl StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin),
    ws_sink: &mut (impl SinkExt<Message> + Unpin),
    tcp_read: &mut (impl AsyncReadExt + Unpin),
    tcp_write: &mut (impl AsyncWriteExt + Unpin),
) {
    let ws_to_tcp = async {
        while let Some(msg) = ws_stream.next().await {
            match msg {
                Ok(Message::Binary(data)) => {
                    if tcp_write.write_all(&data).await.is_err() {
                        break;
                    }
                }
                Ok(Message::Close(_)) | Err(_) => break,
                _ => {}
            }
        }
    };

    let tcp_to_ws = async {
        let mut buf = vec![0u8; 32 * 1024];
        loop {
            let n = match tcp_read.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
            if ws_sink.send(Message::Binary(buf[..n].to_vec().into())).await.is_err() {
                break;
            }
        }
    };

    tokio::select! {
        _ = ws_to_tcp => {}
        _ = tcp_to_ws => {}
    }
}

/// Check Authorization header from raw HTTP request text
fn check_auth_raw(head: &str, token: &str) -> bool {
    for line in head.lines().skip(1) {
        let line = line.trim();
        if line.is_empty() {
            break;
        }
        let lower = line.to_lowercase();
        if !lower.starts_with("authorization:") {
            continue;
        }
        let val = line.splitn(2, ':').nth(1).unwrap_or("").trim();
        if let Some(bearer) = val.strip_prefix("Bearer ") {
            return bearer.trim() == token;
        }
        if let Some(encoded) = val.strip_prefix("Basic ") {
            if let Ok(decoded) = STANDARD.decode(encoded.trim()) {
                if let Ok(s) = std::str::from_utf8(&decoded) {
                    let pass = s.splitn(2, ':').nth(1).unwrap_or(s);
                    return pass == token;
                }
            }
        }
    }
    false
}

async fn send_error(mut stream: TcpStream, code: u16, body: &str) -> Result<(), BoxError> {
    let status = match code {
        400 => "400 Bad Request",
        401 => "401 Unauthorized",
        _ => "500 Internal Server Error",
    };
    let resp = format!(
        "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    stream.write_all(resp.as_bytes()).await?;
    Ok(())
}
