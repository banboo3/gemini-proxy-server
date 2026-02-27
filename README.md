# Gemini Proxy Server

A lightweight WebSocket tunnel server written in Rust, designed to be deployed on [Railway](https://railway.app). Relays TCP traffic between the Gemini Next Desktop client and Google via WebSocket binary frames, with token-based authentication.

## How It Works

```
Gemini Next Desktop (Windows)
  ↓ WebSocket upgrade: GET /tunnel?target=gemini.google.com:443
  ↓ Authorization: Bearer TOKEN
Railway Proxy Server (101 Switching Protocols)
  ↓ TCP connect to target
gemini.google.com
```

The server exposes two routes:

| Route | Method | Auth | Purpose |
|-------|--------|------|---------|
| `/health` | GET | No | Health check / wake-up ping |
| `/tunnel?target=host:port` | GET | Bearer token | WebSocket upgrade → TCP tunnel |

After the WebSocket handshake completes, the server connects to the target via TCP and relays data bidirectionally: WebSocket binary frames ↔ raw TCP bytes.

## Deploy to Railway

### 1. Push to GitHub

```bash
cd gemini-proxy-server
git init
git add .
git commit -m "feat: initial proxy server"
gh repo create gemini-proxy-server --public --push
```

### 2. Create Railway project

1. Go to [railway.app](https://railway.app) and sign in
2. Click **New Project → Deploy from GitHub repo**
3. Select your `gemini-proxy-server` repository
4. Railway detects the `Dockerfile` and builds automatically

### 3. Set environment variables

In the Railway project dashboard, go to **Variables** and add:

| Variable | Value |
|---|---|
| `PORT` | `8080` |
| `PROXY_TOKEN` | A long random string (your secret token) |

Generate a secure token:
```bash
openssl rand -hex 32
```

### 4. Get your Railway domain

After deployment, Railway assigns a domain like `your-app.railway.app`. Note this down — you'll need it for the client configuration.

### 5. Configure the client

In Gemini Next Desktop settings, set the Proxy Server field to:
```
http://user:YOUR_TOKEN@your-app.up.railway.app
```

The client connects to Railway over TLS on port 443 and upgrades to WebSocket at `/tunnel?target=gemini.google.com:443`.

## Environment Variables

| Variable | Default | Description |
|---|---|---|
| `PORT` | `8080` | Port to listen on (Railway sets this automatically) |
| `PROXY_TOKEN` | *(empty)* | Auth token. If empty, the proxy is open to anyone — always set this in production |
| `RUST_LOG` | `info` | Log level (`error`, `warn`, `info`, `debug`) |

## Build Locally

```bash
cargo build --release
PROXY_TOKEN=mytoken PORT=8080 ./target/release/gemini-proxy-server
```

Test with curl:
```bash
# Health check
curl https://your-app.up.railway.app/health

# WebSocket tunnel (via websocat for testing)
websocat -H "Authorization: Bearer mytoken" \
  wss://your-app.up.railway.app/tunnel?target=httpbin.org:443
```

## Security Notes

- Always set `PROXY_TOKEN` — without it anyone can use your proxy
- Use a high-entropy token (32+ random hex characters)
- Railway enforces HTTPS on its public domain, so the Bearer token is encrypted in transit
- The WebSocket tunnel carries raw TCP bytes — Railway's HTTP reverse proxy sees only the initial upgrade request
