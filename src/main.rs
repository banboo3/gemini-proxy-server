use base64::{engine::general_purpose::STANDARD, Engine};
use sha1::{Digest, Sha1};
use futures_util::{SinkExt, StreamExt};
use http_body_util::{combinators::BoxBody, BodyExt, Empty, Full};
use hyper::{
    body::{Bytes, Incoming},
    header,
    server::conn::http1,
    service::service_fn,
    Method, Request, Response, StatusCode,
};
use hyper_util::rt::TokioIo;
use std::net::SocketAddr;
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::{tungstenite::protocol::Message, WebSocketStream};
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
            let io = TokioIo::new(stream);
            if let Err(e) = http1::Builder::new()
                .preserve_header_case(true)
                .title_case_headers(true)
                .serve_connection(
                    io,
                    service_fn(move |req| handle(req, token.clone(), peer)),
                )
                .with_upgrades()
                .await
            {
                let msg = e.to_string();
                if !msg.contains("connection reset")
                    && !msg.contains("broken pipe")
                    && !msg.contains("Connection reset")
                {
                    error!("Connection error from {peer}: {e}");
                }
            }
        });
    }
}

async fn handle(
    req: Request<Incoming>,
    token: String,
    peer: SocketAddr,
) -> Result<Response<BoxBody<Bytes, BoxError>>, BoxError> {
    let path = req.uri().path().to_string();

    // Health check — no auth required
    if path == "/health" && req.method() == Method::GET {
        return Ok(Response::builder()
            .status(StatusCode::OK)
            .body(full_body("ok"))
            .unwrap());
    }

    // WebSocket tunnel — auth required
    if path == "/tunnel" && req.method() == Method::GET {
        if !token.is_empty() && !is_authorized(&req, &token) {
            warn!("Unauthorized tunnel request from {peer}");
            return Ok(Response::builder()
                .status(StatusCode::UNAUTHORIZED)
                .body(full_body("unauthorized"))
                .unwrap());
        }
        return handle_tunnel(req, peer).await;
    }

    // Everything else → 404
    Ok(Response::builder()
        .status(StatusCode::NOT_FOUND)
        .body(full_body("not found"))
        .unwrap())
}

/// Handle WebSocket upgrade at /tunnel?target=host:port
async fn handle_tunnel(
    req: Request<Incoming>,
    peer: SocketAddr,
) -> Result<Response<BoxBody<Bytes, BoxError>>, BoxError> {
    // Extract target from query string
    let target = req
        .uri()
        .query()
        .and_then(|q| {
            q.split('&')
                .find_map(|p| p.strip_prefix("target="))
        })
        .unwrap_or("")
        .to_string();

    if target.is_empty() {
        return Ok(Response::builder()
            .status(StatusCode::BAD_REQUEST)
            .body(full_body("missing target query param"))
            .unwrap());
    }

    info!("Tunnel request for {target} from {peer}");

    // Perform the WebSocket upgrade using hyper's upgrade mechanism
    let (response, fut) = hyper_ws_upgrade(req)?;

    tokio::spawn(async move {
        match fut.await {
            Ok(upgraded) => {
                if let Err(e) = ws_tunnel(upgraded, &target).await {
                    let msg = e.to_string();
                    if !msg.contains("connection reset") && !msg.contains("broken pipe") {
                        error!("Tunnel error for {target}: {e}");
                    }
                }
            }
            Err(e) => error!("Upgrade error for {target}: {e}"),
        }
    });

    Ok(response)
}

/// Build a 101 Switching Protocols response and return a future for the upgraded connection.
/// Takes ownership of the request because `hyper::upgrade::on` requires it.
fn hyper_ws_upgrade(
    req: Request<Incoming>,
) -> Result<(
    Response<BoxBody<Bytes, BoxError>>,
    impl std::future::Future<Output = Result<hyper::upgrade::Upgraded, hyper::Error>>,
), BoxError> {
    // Validate WebSocket upgrade headers
    let key = req
        .headers()
        .get("Sec-WebSocket-Key")
        .ok_or("missing Sec-WebSocket-Key")?
        .to_str()?
        .to_string();

    // Compute accept key per RFC 6455
    let mut hasher = Sha1::new();
    hasher.update(key.as_bytes());
    hasher.update(b"258EAFA5-E914-47DA-95CA-5AB5FB80F65B");
    let accept = STANDARD.encode(hasher.finalize());

    let upgrade_fut = hyper::upgrade::on(req);

    let response = Response::builder()
        .status(StatusCode::SWITCHING_PROTOCOLS)
        .header(header::UPGRADE, "websocket")
        .header(header::CONNECTION, "Upgrade")
        .header("Sec-WebSocket-Accept", accept)
        .body(empty())
        .unwrap();

    Ok((response, upgrade_fut))
}

/// Bidirectional relay: WebSocket binary frames ↔ TCP bytes
async fn ws_tunnel(
    upgraded: hyper::upgrade::Upgraded,
    target: &str,
) -> Result<(), BoxError> {
    // Connect to the actual target (e.g. gemini.google.com:443)
    let tcp = TcpStream::connect(target).await?;
    let (mut tcp_read, mut tcp_write) = tokio::io::split(tcp);

    // Wrap the upgraded connection as a WebSocket server stream
    let ws = WebSocketStream::from_raw_socket(
        TokioIo::new(upgraded),
        tokio_tungstenite::tungstenite::protocol::Role::Server,
        None,
    )
    .await;
    let (mut ws_sink, mut ws_stream) = ws.split();

    info!("Tunnel established to {target}");

    // WS → TCP: read binary frames from client, write to target
    let ws_to_tcp = async {
        while let Some(msg) = ws_stream.next().await {
            match msg {
                Ok(Message::Binary(data)) => {
                    if tokio::io::AsyncWriteExt::write_all(&mut tcp_write, &data).await.is_err() {
                        break;
                    }
                }
                Ok(Message::Close(_)) | Err(_) => break,
                _ => {} // ignore ping/pong/text
            }
        }
    };

    // TCP → WS: read bytes from target, send as binary frames
    let tcp_to_ws = async {
        let mut buf = vec![0u8; 32 * 1024];
        loop {
            let n = match tokio::io::AsyncReadExt::read(&mut tcp_read, &mut buf).await {
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

    Ok(())
}

/// Check Authorization header against our token
fn is_authorized(req: &Request<Incoming>, token: &str) -> bool {
    let Some(auth) = req.headers().get(header::AUTHORIZATION) else {
        return false;
    };
    let Ok(val) = auth.to_str() else { return false };

    if let Some(bearer) = val.strip_prefix("Bearer ") {
        return bearer == token;
    }
    if let Some(encoded) = val.strip_prefix("Basic ") {
        if let Ok(decoded) = STANDARD.decode(encoded) {
            if let Ok(s) = std::str::from_utf8(&decoded) {
                let pass = s.splitn(2, ':').nth(1).unwrap_or(s);
                return pass == token;
            }
        }
    }
    false
}

fn empty() -> BoxBody<Bytes, BoxError> {
    Empty::<Bytes>::new()
        .map_err(|e| Box::new(e) as BoxError)
        .boxed()
}

fn full_body(s: &str) -> BoxBody<Bytes, BoxError> {
    Full::new(Bytes::from(s.to_string()))
        .map_err(|e| Box::new(e) as BoxError)
        .boxed()
}
