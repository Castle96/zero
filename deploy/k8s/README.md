# Deployment Guide — Rust HTTPS Proxy on Kubernetes

## Overview

This directory contains Kubernetes manifests for deploying the Rust HTTPS reverse proxy. It's designed to replace Traefik/Caddy for **static route configurations** — you define the routes at deploy time via ConfigMap.

## Architecture

```
Internet → LoadBalancer:443
              ↓
        http-server Pod
              ↓
    ┌─────────────────────┐
    │ TLS termination     │  rustls
    │ Route matching      │  longest-prefix
    │ X-Forwarded-For     │  real client IP
    │ HSTS                │  Strict-Transport-Security
    │ Connection draining │  graceful shutdown on SIGTERM
    └─────────────────────┘
              ↓
   Upstream ClusterIP Services (e.g. api-service:3000)
```

## Prerequisites

- Kubernetes 1.24+
- cert-manager installed (for TLS certificate management)
- Container registry access (Docker Hub, ECR, GCR, etc.)

## Quick Start

### 1. Build and push the image

```bash
cd http_server
docker build -t your-registry/http-server:latest .
docker push your-registry/http-server:latest
```

Update `image:` in `deployment.yaml` to point to your image.

### 2. Configure routes

Edit `configmap.yaml` — uncomment and set your proxy routes:

```yaml
[[proxy]]
prefix = "/api"
upstream = "http://api-service:3000"

[[proxy]]
prefix = "/app"
upstream = "http://web-service:8080"
```

### 3. Set up TLS with cert-manager

Create a ClusterIssuer for Let's Encrypt:

```yaml
apiVersion: cert-manager.io/v1
kind: ClusterIssuer
metadata:
  name: letsencrypt-prod
spec:
  acme:
    server: https://acme-v02.api.letsencrypt.org/directory
    email: your-email@example.com
    privateKeySecretRef:
      name: letsencrypt-account-key
    solvers:
      - http01:
          ingress: {}
```

Create a Certificate that writes to `http-server-tls` (the Secret name expected by the deployment):

```yaml
apiVersion: cert-manager.io/v1
kind: Certificate
metadata:
  name: http-server-tls
spec:
  secretName: http-server-tls
  dnsNames:
    - your-domain.com
  issuerRef:
    name: letsencrypt-prod
    kind: ClusterIssuer
```

> **Note:** cert-manager's HTTP-01 challenge requires an Ingress/Service that
> serves `/.well-known/acme-challenge/` on port 80. If you're replacing Traefik
> with this proxy, you'll need to use DNS-01 challenges instead, or temporarily
> keep a minimal Ingress controller for the ACME challenge.

### 4. Deploy

```bash
kubectl apply -f deploy/k8s/configmap.yaml
kubectl apply -f deploy/k8s/deployment.yaml
kubectl apply -f deploy/k8s/service.yaml
```

### 5. Verify

```bash
# Check pods are running
kubectl get pods -l app=http-server

# Check the health endpoint
kubectl port-forward deployment/http-server 8443:443
curl -k https://localhost:8443/health

# Check cert health
curl -k https://localhost:8443/health/cert
```

## Configuration

### Environment variables (overrides config.toml)

| Variable | Purpose |
|----------|---------|
| `CONFIG_PATH` | Path to TOML config (default: `/etc/http-server/config.toml`) |
| `TLS_CERT_PATH` | Path to TLS cert PEM file |
| `TLS_KEY_PATH` | Path to TLS key PEM file |
| `DOMAIN` | Override domain from config file |
| `EMAIL` | Override Let's Encrypt email |
| `HSTS_MAX_AGE` | Override HSTS max-age (0 to disable) |
| `PROXY_ROUTES` | JSON route map (e.g. `{"/api":"http://svc:3000"}`) |

### Resource sizing

| Resource | Minimum | Recommended |
|----------|---------|-------------|
| CPU | 100m | 500m |
| Memory | 32Mi | 128Mi |

At 500m CPU / 128Mi RAM this handles ~5,000 concurrent connections.

### Scaling

```bash
kubectl scale deployment http-server --replicas=3
```

The deployment uses `maxUnavailable: 0` + SIGTERM draining for zero-downtime rolling updates.

## TLS through cert-manager (recommended)

The deployment mounts `http-server-tls` Secret at `/etc/http-server/certs/`.
The `TLS_CERT_PATH` and `TLS_KEY_PATH` env vars point at the mounted files.
The proxy reads these on startup instead of using its built-in ACME client.

This is the **standard K8s approach** — cert-manager handles renewal and the
proxy just reads files. The proxy's built-in ACME certbot integration is better
suited for standalone/VPS deployments, not K8s.

## Without cert-manager (standalone ACME)

If you prefer the proxy's built-in ACME (certbot shell-out), remove the TLS
env vars and cert volume mount, and ensure:
- The pod has certbot + openssl installed (not in the distroless image)
- Port 80 is accessible for HTTP-01 challenges
- `DOMAIN` and `EMAIL` env vars are set
- `USE_STAGING=false` for production certs

This is more complex in K8s and not recommended. Use cert-manager instead.

## Client IP preservation

The Service uses `externalTrafficPolicy: Local` to preserve the real client IP.
Without this, all traffic would show the node IP. The proxy forwards the real
IP via `X-Forwarded-For` and `X-Real-IP` headers to upstream services.

## Graceful shutdown

On `kubectl delete pod` or rolling update:
1. K8s sends SIGTERM
2. The proxy stops accepting new connections
3. **Waits for active connections to finish** (up to 30 seconds)
4. K8s waits 45 seconds (`terminationGracePeriodSeconds`) before SIGKILL

This prevents dropped requests during deployments.
