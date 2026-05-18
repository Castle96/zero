# Rust HTTPS Reverse Proxy

A production-ready HTTPS reverse proxy with automatic TLS, path-based routing, connection draining, and first-class Kubernetes support.

## Quick Start

```bash
# Build
cargo build --release

# Configure (env vars or config.toml)
cp .env.example .env
# Edit .env with your DOMAIN and EMAIL

# Run (requires certbot for ACME, or set TLS_CERT_PATH for managed certs)
cargo run
```

## Features

| Feature | Detail |
|---------|--------|
| **TLS termination** | rustls — modern, audited, no OpenSSL |
| **Let's Encrypt ACME** | Automatic provisioning via certbot (standalone) |
| **External cert support** | Via `TLS_CERT_PATH` / `TLS_KEY_PATH` env vars (K8s cert-manager) |
| **Auto-renewal** | 24h background check, hot-swaps TLS acceptor (zero-downtime) |
| **Path-based reverse proxy** | Longest-prefix matching, full request forwarding |
| **Client IP forwarding** | `X-Forwarded-For`, `X-Forwarded-Proto`, `X-Real-IP` headers |
| **HTTP→HTTPS redirect** | Automatic 301 redirect on port 80 |
| **Connection draining** | Tracks active connections, waits for drain on shutdown |
| **HSTS** | `Strict-Transport-Security: max-age=31536000; includeSubDomains` |
| **CORS** | Permissive (configurable) |
| **Compression** | Transparent gzip response encoding |
| **Request logging** | Method, URI, status, latency via `tracing` |
| **Graceful shutdown** | SIGTERM → drain → stop (K8s-friendly) |
| **Health endpoints** | `GET /health`, `GET /health/cert` |
| **Config file** | TOML (`config.toml`) with env var overrides |
| **Container image** | ~19MB multi-stage Dockerfile (static musl binary) |
| **K8s manifests** | Deployment, Service, ConfigMap, deploy guide |

## Architecture

```
Internet → :443
     ↓
TLS handshake (rustls / tokio-rustls)
     ↓
axum Router
  ├── GET /             → Welcome page (static)
  ├── GET /health       → 200 OK
  ├── GET /health/cert  → Certificate expiry JSON
  ├── /.well-known/acme-challenge/:token  → LE validation
  └── fallback → ReverseProxy → upstream ClusterIP Services
                   ├── Longest-prefix route matching
                   ├── Full method/header/body forwarding
                   ├── X-Forwarded-For, X-Real-IP injection
                   └── 404 if no route matches

Middleware (inner → outer):
  CompressionLayer (gzip)
  → HSTS header (configurable)
  → CorsLayer (permissive)
  → TraceLayer (request/response logging)

Port 80 → HTTP redirect server → 301 to HTTPS
```

## Configuration

### Option A: TOML config file (recommended)

Copy `config.example.toml` to `config.toml` and edit:

```toml
[server]
domain = "example.com"
email = "admin@example.com"
host = "0.0.0.0"
port = 443

[tls]
cert_dir = "certs/"
use_staging = false
hsts_max_age = 31536000

[[proxy]]
prefix = "/api"
upstream = "http://api-service:3000"

[[proxy]]
prefix = "/app"
upstream = "http://web-service:8080"
```

Set `CONFIG_PATH` env var to use a non-default location.

### Option B: Environment variables

Copy `.env.example` to `.env` and edit. Required: `DOMAIN`, `EMAIL`.

Full reference:

| Variable | Default | Description |
|----------|---------|-------------|
| `DOMAIN` | (required) | TLS certificate domain |
| `EMAIL` | (required) | Let's Enctypt contact email |
| `HOST` | `0.0.0.0` | Bind address |
| `PORT` | `443` | TLS listener port |
| `CERT_DIR` | `certs/` | Certificate storage directory |
| `USE_STAGING` | `true` | Let's Encrypt staging environment |
| `TLS_CERT_PATH` | — | Path to cert PEM (external cert management) |
| `TLS_KEY_PATH` | — | Path to key PEM (external cert management) |
| `HSTS_MAX_AGE` | `31536000` | HSTS max-age (seconds; 0 = disable) |
| `HSTS_INCLUDE_SUBDOMAINS` | `true` | Include `includeSubDomains` in HSTS |
| `PROXY_ROUTES` | — | JSON route map (e.g. `{"/api":"http://svc:3000"}`) |
| `CONFIG_PATH` | `config.toml` | Path to TOML config file |

### Certificate sources (determined at startup)

1. **Local cert dir** — `{CERT_DIR}/cert.pem` + `{CERT_DIR}/key.pem`
2. **System certbot** — `/etc/letsencrypt/live/{DOMAIN}/fullchain.pem`
3. **Env var paths** — `TLS_CERT_PATH` + `TLS_KEY_PATH` (for K8s cert-manager)
4. **Auto-provision** — Shells out to certbot (requires certbot + port 80)

When `TLS_CERT_PATH` is set, the built-in ACME renewal loop is skipped entirely
— the proxy assumes an external system manages certificate rotation.

## Docker

```bash
# Build
docker build -t http-server:latest .

# Run with external certs
docker run --rm -p 443:443 -p 80:80 \
  -v /path/to/certs:/certs:ro \
  -e TLS_CERT_PATH=/certs/cert.pem \
  -e TLS_KEY_PATH=/certs/key.pem \
  -e PROXY_ROUTES='{"/api":"http://upstream:3000"}' \
  http-server:latest
```

Image size: ~19MB (multistage, musl, fully static).

## Kubernetes Deployment

Full manifests and guide in `deploy/k8s/`.

```bash
kubectl apply -f deploy/k8s/
```

The deployment:
- Uses cert-manager for TLS (standard K8s pattern)
- 2 replicas with rolling updates (zero-downtime)
- TCP health probes on port 443
- `externalTrafficPolicy: Local` for client IP preservation
- 45s termination grace period for connection draining

## Development

```bash
# Test locally (no root needed)
PORT=8443 \
TLS_CERT_PATH=test-certs/cert.pem \
TLS_KEY_PATH=test-certs/key.pem \
PROXY_ROUTES='{"/":"http://localhost:8080"}' \
cargo run

# Format, lint, test
cargo fmt
cargo clippy --all-targets --all-features
cargo test
cargo build --release
```

To generate self-signed test certs:
```bash
mkdir -p test-certs
openssl req -x509 -newkey rsa:4096 \
  -keyout test-certs/key.pem \
  -out test-certs/cert.pem \
  -days 365 -nodes \
  -subj "/CN=localhost" \
  -addext "subjectAltName=DNS:localhost,IP:127.0.0.1"
```

## Project Structure

```
src/
├── main.rs          # Entrypoint, accept loop, connection draining, SIGTERM
├── config.rs        # TOML + env var config loader
├── tls.rs           # rustls PEM loading, TlsAcceptor factory
├── acme.rs          # Let's Encrypt certbot integration
├── cert_manager.rs  # 24h renewal loop with hot-swap
├── http_redirect.rs # Port 80 → 301 redirect to HTTPS
├── proxy.rs         # Reverse proxy: routing, forwarding, client IP
└── lib.rs           # Module declarations
deploy/k8s/
├── configmap.yaml   # Route configuration
├── deployment.yaml  # 2-replica deployment with probes
├── service.yaml     # LoadBalancer, client IP preservation
└── README.md        # K8s deployment guide
```

## License

MIT
