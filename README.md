# Gemini Proxy Server

A lightweight HTTP/HTTPS proxy server written in Rust, designed to be deployed on [Railway](https://railway.app). Acts as a relay between the Gemini Next Desktop client and Google, with token-based authentication.

## How It Works

```
Gemini Next Desktop (Windows)
  ↓ CONNECT tunnel (with Proxy-Authorization token)
Railway Proxy Server
  ↓
gemini.google.com
```

The server handles:
- `CONNECT` tunnels for HTTPS traffic (covers WebSocket and SSE streaming too)
- Plain HTTP forwarding
- Token authentication via `Proxy-Authorization: Basic user:TOKEN`
- Automatic stripping of `X-Forwarded-For` and `Via` headers

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
http://user:YOUR_TOKEN@your-app.railway.app:8080
```

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
curl -x http://user:mytoken@localhost:8080 https://httpbin.org/ip
```

## Security Notes

- Always set `PROXY_TOKEN` — without it anyone can use your proxy
- Use a high-entropy token (32+ random hex characters)
- Railway enforces HTTPS on its public domain, so the token is encrypted in transit
- The server strips `X-Forwarded-For` and `Via` headers so Google cannot detect the proxy hop
