use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use axum::{
    extract::Path,
    http::{header, StatusCode},
    response::{Html, IntoResponse},
    routing::get,
    Json, Router,
};
use hyper::server::conn::http1;
use tokio::net::TcpListener;
use tokio::signal;
use tokio::sync::{broadcast, RwLock};
use tokio::time::timeout;
use tower::util::ServiceExt;
use tower_http::compression::CompressionLayer;
use tower_http::cors::CorsLayer;
use tower_http::set_header::SetResponseHeaderLayer;
use tower_http::trace::TraceLayer;
use tracing::{error, info, warn};

mod acme;
mod cert_manager;
mod config;
mod http_redirect;
mod proxy;
mod tls;

use acme::LetsEncryptConfig;
use anyhow::Result;
use cert_manager::CertificateManager;
use config::ResolvedConfig;
use proxy::ReverseProxy;
use tls::TlsConfig;
use tokio_rustls::TlsAcceptor;

const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(30);

// ---------------------------------------------------------------------------
// Entrypoint
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    let cfg = config::load()?;

    info!("Starting HTTP server for domain: {}", cfg.domain);

    // --- TLS / ACME provisioning ---
    let le_config = LetsEncryptConfig::new(
        cfg.domain.clone(),
        cfg.email.clone(),
        &cfg.cert_dir,
        cfg.use_staging,
    );

    let tls_config = TlsConfig::load_from_le_config(&le_config)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to load TLS config: {}", e))?;
    let initial_acceptor = Arc::new(
        tls_config
            .create_acceptor()
            .map_err(|e| anyhow::anyhow!("Failed to create acceptor: {}", e))?,
    );

    let shared_acceptor: Arc<RwLock<Arc<TlsAcceptor>>> =
        Arc::new(RwLock::new(initial_acceptor.clone()));

    let (shutdown_tx, _) = broadcast::channel(1);

    // --- Background tasks ---
    // If TLS_CERT_PATH is set, the user is managing certs externally
    // (e.g., via cert-manager in K8s) — skip the built-in ACME renewal loop.
    let use_builtin_acme = std::env::var("TLS_CERT_PATH").is_err();

    let cert_manager_handle = if use_builtin_acme {
        let cert_manager = CertificateManager::new(le_config.clone(), shared_acceptor.clone());
        Some(tokio::spawn(async move {
            if let Err(e) = cert_manager.start_renewal_loop().await {
                error!("Certificate manager loop failed: {}", e);
            }
        }))
    } else {
        info!("TLS_CERT_PATH is set — skipping built-in ACME renewal (using external cert management)");
        None
    };

    let http_redirect_handle = tokio::spawn(async {
        if let Err(e) = http_redirect::start_http_redirect_server().await {
            error!("HTTP redirect server failed: {}", e);
        }
    });

    tokio::time::sleep(Duration::from_secs(2)).await;

    // --- Application router ---
    let proxy_state = Arc::new(ReverseProxy::from_routes(cfg.proxy_routes.clone()).await);
    let app = create_app(proxy_state, &cfg);

    let addr = format!("{}:{}", cfg.host, cfg.port);
    let listener = TcpListener::bind(&addr).await?;
    info!("HTTPS server listening on https://{}", addr);

    // --- Accept loop (with active connection tracking) ---
    let active_connections = Arc::new(AtomicUsize::new(0));
    let mut shutdown_rx = shutdown_tx.subscribe();
    let mut sigterm = signal::unix::signal(signal::unix::SignalKind::terminate())?;

    tokio::select! {
        result = async {
            loop {
                tokio::select! {
                    _ = shutdown_rx.recv() => {
                        info!("Shutdown signal received, stopping listener");
                        break;
                    }
                    _ = sigterm.recv() => {
                        info!("SIGTERM received (Kubernetes shutdown), stopping listener");
                        break;
                    }
                    result = listener.accept() => {
                        match result {
                            Ok((socket, peer_addr)) => {
                                active_connections.fetch_add(1, Ordering::SeqCst);
                                let shared_acceptor = shared_acceptor.clone();
                                let app = app.clone();
                                let active = active_connections.clone();

                                tokio::spawn(async move {
                                    let _guard = ConnectionGuard(active);
                                    if let Err(e) = handle_connection(
                                        socket, peer_addr, shared_acceptor, app,
                                    ).await {
                                        error!("Connection error: {}", e);
                                    }
                                });
                            }
                            Err(e) => {
                                error!("Failed to accept connection: {}", e);
                            }
                        }
                    }
                }
            }
            Ok::<(), anyhow::Error>(())
        } => {
            if let Err(e) = result {
                error!("Server error: {}", e);
            }
        }
    }

    // --- Graceful drain ---
    info!(
        "Draining {} active connection(s)...",
        active_connections.load(Ordering::SeqCst)
    );

    match timeout(SHUTDOWN_TIMEOUT, async {
        while active_connections.load(Ordering::SeqCst) > 0 {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    {
        Ok(()) => info!("All connections drained cleanly"),
        Err(_) => warn!(
            "Drain timeout reached, {} connection(s) still active",
            active_connections.load(Ordering::SeqCst)
        ),
    }

    // --- Stop background tasks ---
    let _ = shutdown_tx.send(());
    if let Some(handle) = cert_manager_handle {
        handle.abort();
    }
    http_redirect_handle.abort();
    info!("Server shutdown complete");
    Ok(())
}

// ---------------------------------------------------------------------------
// Connection handling
// ---------------------------------------------------------------------------

/// Drop guard that decrements the active-connection counter.
struct ConnectionGuard(Arc<AtomicUsize>);

impl Drop for ConnectionGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

async fn handle_connection(
    socket: tokio::net::TcpStream,
    peer_addr: std::net::SocketAddr,
    shared_acceptor: Arc<RwLock<Arc<TlsAcceptor>>>,
    app: Router,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let acceptor = {
        let guard = shared_acceptor.read().await;
        guard.clone()
    };

    // TLS handshake (10s timeout)
    let tls_stream = timeout(Duration::from_secs(10), acceptor.accept(socket))
        .await
        .map_err(|_| anyhow::anyhow!("TLS handshake timeout"))??;

    info!("TLS connection established from {}", peer_addr);

    let io = hyper_util::rt::TokioIo::new(tls_stream);

    // Inject the real client IP into every request so axum handlers
    // and the proxy can read it for X-Forwarded-* headers.
    let hyper_service = hyper::service::service_fn(move |mut req| {
        req.extensions_mut().insert(peer_addr);
        let app = app.clone();
        async move {
            match app.oneshot(req).await {
                Ok(resp) => Ok::<_, std::convert::Infallible>(resp.into_response()),
                Err(_) => Ok::<_, std::convert::Infallible>(
                    (StatusCode::INTERNAL_SERVER_ERROR, "Internal error").into_response(),
                ),
            }
        }
    });

    timeout(
        Duration::from_secs(60),
        http1::Builder::new().serve_connection(io, hyper_service),
    )
    .await
    .map_err(|_| anyhow::anyhow!("Connection timeout"))??;

    Ok(())
}

// ---------------------------------------------------------------------------
// Router and handlers
// ---------------------------------------------------------------------------

/// Build the application router with routes, proxy fallback, and middleware.
fn create_app(proxy_state: Arc<ReverseProxy>, cfg: &ResolvedConfig) -> Router {
    let mut router = Router::new()
        .route("/", get(handler_root))
        .route("/health", get(handler_health))
        .route("/health/cert", get(handler_cert_health))
        .route(
            "/.well-known/acme-challenge/:token",
            get(handle_acme_challenge),
        )
        .fallback(proxy::proxy_handler)
        .with_state(proxy_state)
        .layer(TraceLayer::new_for_http())
        .layer(CorsLayer::permissive())
        .layer(CompressionLayer::new());

    // Conditionally add HSTS (disabled if max-age is 0)
    if !cfg.hsts_value.is_empty() {
        router = router.layer(SetResponseHeaderLayer::overriding(
            header::STRICT_TRANSPORT_SECURITY,
            axum::http::HeaderValue::from_str(&cfg.hsts_value).expect("Invalid HSTS header value"),
        ));
    }

    router
}

async fn handler_root() -> Html<&'static str> {
    Html("<h1>Welcome to Rust HTTPS Server with Let's Encrypt!</h1>")
}

async fn handler_health() -> impl IntoResponse {
    (StatusCode::OK, "OK")
}

async fn handler_cert_health() -> impl IntoResponse {
    use chrono::Utc;

    let cert_dir = std::path::Path::new("certs/");
    let metadata_path = cert_dir.join("cert_metadata.json");

    match std::fs::read_to_string(&metadata_path) {
        Ok(content) => match serde_json::from_str::<acme::CertificateMetadata>(&content) {
            Ok(metadata) => {
                let now = Utc::now();
                let days_until_expiry = (metadata.expires_at - now).num_days();
                let status = if days_until_expiry > 30 {
                    "healthy"
                } else if days_until_expiry > 7 {
                    "renewing_soon"
                } else {
                    "critical"
                };

                let body = serde_json::json!({
                    "status": status,
                    "domain": metadata.domain,
                    "expires_at": metadata.expires_at,
                    "days_until_expiry": days_until_expiry,
                });

                Json(body).into_response()
            }
            Err(_) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to parse metadata",
            )
                .into_response(),
        },
        Err(_) => (StatusCode::SERVICE_UNAVAILABLE, "No certificate found").into_response(),
    }
}

async fn handle_acme_challenge(Path(token): Path<String>) -> Result<String, StatusCode> {
    let cert_dir = std::path::Path::new("certs/");
    let challenge_file = cert_dir
        .join(".well-known")
        .join("acme-challenge")
        .join(&token);

    match std::fs::read_to_string(&challenge_file) {
        Ok(content) => Ok(content),
        Err(_) => Err(StatusCode::NOT_FOUND),
    }
}
