use std::collections::HashMap;
use std::sync::Arc;

use axum::{
    body::Body,
    extract::{Request, State},
    http::{header, HeaderMap, Method, StatusCode},
    response::{IntoResponse, Response},
};
use http_body_util::BodyExt;
use reqwest::Client;
use tokio::sync::RwLock;
use tracing::{debug, error, info, warn};

/// Hop-by-hop headers that must not be forwarded by a proxy
/// (RFC 9113 §8.2.2 and RFC 7230 §6.1).
const HOP_BY_HOP: &[header::HeaderName] = &[
    header::CONNECTION,
    header::TRANSFER_ENCODING,
    header::HOST,
    header::PROXY_AUTHENTICATE,
    header::PROXY_AUTHORIZATION,
    header::TE,
    header::TRAILER,
    header::UPGRADE,
];

/// A path-based reverse proxy with hot-reloadable routes and a reused HTTP client.
#[derive(Clone)]
pub struct ReverseProxy {
    routes: Arc<RwLock<HashMap<String, String>>>,
    client: Client,
}

impl ReverseProxy {
    /// Create an empty proxy. Use [`add_route`](Self::add_route) or
    /// [`from_env`](Self::from_env) to populate routes.
    pub fn new() -> Self {
        Self {
            routes: Arc::new(RwLock::new(HashMap::new())),
            client: Client::builder()
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .expect("Failed to create reqwest client"),
        }
    }

    /// Register a route prefix → upstream base URL mapping.
    ///
    /// Requests whose path starts with `prefix` are forwarded to `upstream`
    /// with the remainder of the path appended. E.g. route `"/api" → "http://localhost:3000"`
    /// means a request to `/api/users` becomes `http://localhost:3000/users`.
    #[allow(dead_code)]
    pub async fn add_route(&self, prefix: &str, upstream: &str) {
        let mut routes = self.routes.write().await;
        routes.insert(prefix.to_string(), upstream.to_string());
        info!("Added proxy route: {} -> {}", prefix, upstream);
    }

    /// Load routes from a pre-built HashMap (e.g. from config file).
    pub async fn from_routes(routes: HashMap<String, String>) -> Self {
        let proxy = Self::new();
        {
            let mut locked = proxy.routes.write().await;
            for (prefix, upstream) in &routes {
                info!("Proxy route: {} -> {}", prefix, upstream);
                locked.insert(prefix.clone(), upstream.clone());
            }
        }
        if !routes.is_empty() {
            info!("Loaded {} proxy route(s) from config", routes.len());
        }
        proxy
    }

    /// Load routes from the `PROXY_ROUTES` environment variable.
    ///
    /// Expects a JSON object: `{"/api": "http://localhost:3000", "/app": "http://localhost:5173"}`
    #[allow(dead_code)]
    pub async fn from_env() -> Self {
        let proxy = Self::new();
        match std::env::var("PROXY_ROUTES") {
            Ok(json_str) => match serde_json::from_str::<HashMap<String, String>>(&json_str) {
                Ok(routes) => {
                    let mut locked = proxy.routes.write().await;
                    for (prefix, upstream) in &routes {
                        info!("Proxy route: {} -> {}", prefix, upstream);
                        locked.insert(prefix.clone(), upstream.clone());
                    }
                    info!("Loaded {} proxy route(s) from PROXY_ROUTES", routes.len());
                }
                Err(e) => {
                    warn!(
                        "Failed to parse PROXY_ROUTES: {}. \
                         Expected JSON like {{\"/api\":\"http://localhost:3000\"}}",
                        e
                    );
                }
            },
            Err(std::env::VarError::NotPresent) => {
                debug!("PROXY_ROUTES not set — no proxy routes configured");
            }
            Err(std::env::VarError::NotUnicode(_)) => {
                warn!("PROXY_ROUTES is not valid UTF-8");
            }
        }
        proxy
    }

    /// Look up the upstream for a path using longest-prefix matching.
    pub async fn get_upstream(&self, path: &str) -> Option<String> {
        let routes = self.routes.read().await;
        routes
            .iter()
            .filter(|(prefix, _)| path.starts_with(*prefix))
            .max_by_key(|(prefix, _)| prefix.len())
            .map(|(_, upstream)| upstream.clone())
    }
}

impl Default for ReverseProxy {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Axum handler — use as Router::fallback
// ---------------------------------------------------------------------------

/// Axum handler that proxies unmatched requests through registered upstreams.
///
/// Usage:
/// ```ignore
/// let proxy = Arc::new(ReverseProxy::from_env().await);
/// Router::new()
///     .route("/health", get(health))
///     .fallback(proxy::proxy_handler)
///     .with_state(proxy);
/// ```
pub async fn proxy_handler(State(proxy): State<Arc<ReverseProxy>>, req: Request<Body>) -> Response {
    // Extract path string first to avoid borrowing req when we move it
    let path = req
        .uri()
        .path_and_query()
        .map(|pq| pq.as_str().to_owned())
        .unwrap_or_else(|| req.uri().path().to_owned());

    let upstream = match proxy.get_upstream(&path).await {
        Some(u) => u,
        None => {
            debug!("No proxy route for {}", path);
            return (StatusCode::NOT_FOUND, "Not Found").into_response();
        }
    };

    // Forward the full request
    match forward(&proxy, req, &upstream).await {
        Ok(resp) => {
            debug!("Proxy {} -> {} -> {}", path, upstream, resp.status());
            resp
        }
        Err(e) => {
            error!("Proxy error for {} -> {}: {}", path, upstream, e);
            (StatusCode::BAD_GATEWAY, format!("Bad Gateway: {}", e)).into_response()
        }
    }
}

// ---------------------------------------------------------------------------
// Internal: build and send the proxied request
// ---------------------------------------------------------------------------

async fn forward(
    proxy: &ReverseProxy,
    req: Request<Body>,
    upstream_base: &str,
) -> Result<Response, anyhow::Error> {
    let (parts, body) = req.into_parts();

    // Read the full request body
    let body_bytes = body
        .collect()
        .await
        .map_err(|e| anyhow::anyhow!("Failed to read request body: {}", e))?
        .to_bytes();

    // Assemble the upstream URL
    let path_and_query = parts
        .uri
        .path_and_query()
        .map(|pq| pq.as_str())
        .unwrap_or(parts.uri.path());
    let upstream_url = format!("{}{}", upstream_base.trim_end_matches('/'), path_and_query);

    debug!(
        "Forwarding {} {} -> {} ({} bytes body)",
        parts.method,
        parts.uri,
        upstream_url,
        body_bytes.len()
    );

    // Build the reqwest request, preserving method
    let mut req_builder = match parts.method {
        Method::GET => proxy.client.get(&upstream_url),
        Method::POST => proxy.client.post(&upstream_url),
        Method::PUT => proxy.client.put(&upstream_url),
        Method::DELETE => proxy.client.delete(&upstream_url),
        Method::PATCH => proxy.client.patch(&upstream_url),
        Method::HEAD => proxy.client.head(&upstream_url),
        other => proxy.client.request(other, &upstream_url),
    };

    // Forward headers (strip hop-by-hop)
    let filtered = filter_headers(&parts.headers);
    req_builder = req_builder.headers(filtered);

    // Add client-IP forwarding headers so upstreams see the real client
    let client_ip = parts
        .extensions
        .get::<std::net::SocketAddr>()
        .map(|addr| addr.ip().to_string())
        .unwrap_or_else(|| "unknown".to_string());

    req_builder = req_builder
        .header("X-Forwarded-For", &client_ip)
        .header("X-Forwarded-Proto", "https")
        .header("X-Real-IP", &client_ip);

    // Attach body
    if !body_bytes.is_empty() {
        req_builder = req_builder.body(body_bytes.to_vec());
    }

    // Send
    let upstream_resp = req_builder
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("Upstream request failed: {}", e))?;

    // Convert response back to axum format
    let status = StatusCode::from_u16(upstream_resp.status().as_u16())
        .map_err(|e| anyhow::anyhow!("Invalid upstream status: {}", e))?;

    let mut response_headers = HeaderMap::new();
    for (key, value) in upstream_resp.headers() {
        if !HOP_BY_HOP.contains(key) {
            response_headers.insert(key.clone(), value.clone());
        }
    }

    let resp_body = upstream_resp
        .bytes()
        .await
        .map_err(|e| anyhow::anyhow!("Failed to read upstream response: {}", e))?;

    let mut response = Response::new(Body::from(resp_body.to_vec()));
    *response.status_mut() = status;
    *response.headers_mut() = response_headers;

    Ok(response)
}

/// Return a copy of `headers` with all hop-by-hop headers removed.
fn filter_headers(headers: &HeaderMap) -> HeaderMap {
    let mut filtered = HeaderMap::new();
    for (key, value) in headers {
        if !HOP_BY_HOP.contains(key) {
            filtered.insert(key.clone(), value.clone());
        }
    }
    filtered
}
