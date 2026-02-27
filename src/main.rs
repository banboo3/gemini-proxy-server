use base64::{engine::general_purpose::STANDARD, Engine};
use http_body_util::{combinators::BoxBody, BodyExt, Empty};
use hyper::{
    body::{Bytes, Incoming},
    server::conn::http1,
    service::service_fn,
    Method, Request, Response, StatusCode,
};
use hyper_util::rt::TokioIo;
use std::net::SocketAddr;
use tokio::net::{TcpListener, TcpStream};
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
                // Ignore connection reset / broken pipe
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
    // Auth check
    if !token.is_empty() && !is_authorized(&req, &token) {
        warn!("Unauthorized request from {peer}");
        let mut resp = Response::new(empty());
        *resp.status_mut() = StatusCode::PROXY_AUTHENTICATION_REQUIRED;
        resp.headers_mut().insert(
            "Proxy-Authenticate",
            "Basic realm=\"GeminiProxy\"".parse().unwrap(),
        );
        return Ok(resp);
    }

    if req.method() == Method::CONNECT {
        handle_connect(req, peer).await
    } else {
        handle_http(req, peer).await
    }
}

/// HTTPS tunnel via CONNECT
async fn handle_connect(
    req: Request<Incoming>,
    peer: SocketAddr,
) -> Result<Response<BoxBody<Bytes, BoxError>>, BoxError> {
    let host = req
        .uri()
        .authority()
        .map(|a| a.to_string())
        .unwrap_or_default();

    info!("CONNECT {host} from {peer}");

    tokio::task::spawn(async move {
        match hyper::upgrade::on(req).await {
            Ok(upgraded) => {
                if let Err(e) = tunnel(upgraded, &host).await {
                    let msg = e.to_string();
                    if !msg.contains("connection reset") && !msg.contains("broken pipe") {
                        error!("Tunnel error for {host}: {e}");
                    }
                }
            }
            Err(e) => error!("Upgrade error for {host}: {e}"),
        }
    });

    Ok(Response::new(empty()))
}

/// Bidirectional TCP tunnel between upgraded connection and target
async fn tunnel(
    upgraded: hyper::upgrade::Upgraded,
    host: &str,
) -> Result<(), BoxError> {
    let mut server = TcpStream::connect(host).await?;
    let mut client = TokioIo::new(upgraded);

    // Return 200 Connection Established before tunneling
    // (hyper sends this automatically when we return 200 from handle_connect)
    tokio::io::copy_bidirectional(&mut client, &mut server).await?;
    Ok(())
}

/// Plain HTTP forwarding
async fn handle_http(
    mut req: Request<Incoming>,
    peer: SocketAddr,
) -> Result<Response<BoxBody<Bytes, BoxError>>, BoxError> {
    let uri = req.uri().clone();
    let host = uri
        .host()
        .ok_or("Missing host in HTTP request")?
        .to_string();
    let port = uri.port_u16().unwrap_or(80);
    let addr = format!("{host}:{port}");

    info!("HTTP {} {uri} from {peer}", req.method());

    // Strip hop-by-hop and proxy headers before forwarding
    strip_hop_headers(req.headers_mut());
    req.headers_mut().remove("proxy-authorization");

    let stream = TcpStream::connect(&addr).await?;
    let io = TokioIo::new(stream);

    let (mut sender, conn) =
        hyper::client::conn::http1::Builder::new()
            .preserve_header_case(true)
            .title_case_headers(true)
            .handshake(io)
            .await?;

    tokio::spawn(async move {
        if let Err(e) = conn.await {
            error!("HTTP client conn error: {e}");
        }
    });

    let resp = sender.send_request(req).await?;
    Ok(resp.map(|b| b.map_err(|e| Box::new(e) as BoxError).boxed()))
}

/// Check Proxy-Authorization header against our token
fn is_authorized(req: &Request<Incoming>, token: &str) -> bool {
    let Some(auth) = req.headers().get("proxy-authorization") else {
        return false;
    };
    let Ok(val) = auth.to_str() else { return false };

    // Support both "Basic base64(user:token)" and bare "Bearer token"
    if let Some(encoded) = val.strip_prefix("Basic ") {
        if let Ok(decoded) = STANDARD.decode(encoded) {
            if let Ok(s) = std::str::from_utf8(&decoded) {
                // user:token — we only check the password part
                let pass = s.splitn(2, ':').nth(1).unwrap_or(s);
                return pass == token;
            }
        }
    }
    if let Some(bearer) = val.strip_prefix("Bearer ") {
        return bearer == token;
    }
    false
}

/// Remove hop-by-hop headers that must not be forwarded
fn strip_hop_headers(headers: &mut hyper::HeaderMap) {
    for key in &[
        "connection", "keep-alive", "proxy-authenticate",
        "proxy-authorization", "te", "trailers",
        "transfer-encoding", "upgrade",
    ] {
        headers.remove(*key);
    }
}

fn empty() -> BoxBody<Bytes, BoxError> {
    Empty::<Bytes>::new()
        .map_err(|e| Box::new(e) as BoxError)
        .boxed()
}
