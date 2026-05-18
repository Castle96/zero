use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};
use serde::Deserialize;
use tracing::info;

// ---------------------------------------------------------------------------
// Top-level config
// ---------------------------------------------------------------------------

/// Server-wide configuration, loaded from `config.toml` (optional) and
/// overridden by environment variables for 12-factor-app compatibility.
#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub server: ServerConfig,
    pub tls: Option<TlsConfig>,
    pub proxy: Option<Vec<ProxyRoute>>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ServerConfig {
    pub domain: String,
    pub email: String,
    pub host: Option<String>,
    pub port: Option<u16>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TlsConfig {
    pub cert_dir: Option<String>,
    pub use_staging: Option<bool>,
    /// HSTS max-age in seconds. Set to 0 to disable HSTS entirely.
    pub hsts_max_age: Option<u32>,
    pub hsts_include_subdomains: Option<bool>,
}

/// A single reverse-proxy route mapping.
#[derive(Debug, Clone, Deserialize)]
pub struct ProxyRoute {
    pub prefix: String,
    pub upstream: String,
}

// ---------------------------------------------------------------------------
// Resolved (flattened) config — what the rest of the app uses
// ---------------------------------------------------------------------------

/// Fully-resolved configuration with all defaults applied.
#[derive(Debug, Clone)]
pub struct ResolvedConfig {
    pub domain: String,
    pub email: String,
    pub host: String,
    pub port: u16,
    pub cert_dir: String,
    pub use_staging: bool,
    #[allow(dead_code)]
    pub hsts_max_age: u32,
    pub hsts_value: String,
    pub proxy_routes: HashMap<String, String>,
}

// ---------------------------------------------------------------------------
// Loading
// ---------------------------------------------------------------------------

/// Load and resolve configuration from `config.toml` (if it exists) and
/// environment variables.
///
/// Resolution order (later wins):
///   1. Hard-coded defaults
///   2. `config.toml` values (if file exists)
///   3. Environment variables
pub fn load() -> Result<ResolvedConfig> {
    dotenv::dotenv().ok();

    // --- Step 1: hard-coded defaults ---
    let mut domain: Option<String> = None;
    let mut email: Option<String> = None;
    let mut host: Option<String> = None;
    let mut port: Option<u16> = None;
    let mut cert_dir: Option<String> = None;
    let mut use_staging: Option<bool> = None;
    let mut hsts_max_age: Option<u32> = None;
    let mut hsts_include_subdomains: Option<bool> = None;
    let mut proxy_routes: HashMap<String, String> = HashMap::new();

    // --- Step 2: load from config.toml ---
    let config_path = std::env::var("CONFIG_PATH").unwrap_or_else(|_| "config.toml".into());
    let config_path = Path::new(&config_path);

    if config_path.exists() {
        info!("Loading config from {}", config_path.display());
        let contents = std::fs::read_to_string(config_path)
            .with_context(|| format!("reading {}", config_path.display()))?;
        let file_config: Config = toml::from_str(&contents)
            .with_context(|| format!("parsing {}", config_path.display()))?;

        domain = Some(file_config.server.domain);
        email = Some(file_config.server.email);
        host = file_config.server.host;
        port = file_config.server.port;

        if let Some(tls) = &file_config.tls {
            cert_dir = tls.cert_dir.clone();
            use_staging = tls.use_staging;
            hsts_max_age = tls.hsts_max_age;
            hsts_include_subdomains = tls.hsts_include_subdomains;
        }

        if let Some(routes) = &file_config.proxy {
            for route in routes {
                proxy_routes.insert(route.prefix.clone(), route.upstream.clone());
            }
        }
    }

    // --- Step 3: environment-variable overrides ---
    if let Ok(v) = std::env::var("DOMAIN") {
        domain = Some(v);
    }
    if let Ok(v) = std::env::var("EMAIL") {
        email = Some(v);
    }
    if let Ok(v) = std::env::var("HOST") {
        host = Some(v);
    }
    if let Ok(v) = std::env::var("PORT") {
        port = Some(v.parse().map_err(|_| anyhow::anyhow!("Invalid PORT"))?);
    }
    if let Ok(v) = std::env::var("CERT_DIR") {
        cert_dir = Some(v);
    }
    if let Ok(v) = std::env::var("USE_STAGING") {
        use_staging = Some(v.to_lowercase() == "true");
    }
    if let Ok(v) = std::env::var("HSTS_MAX_AGE") {
        hsts_max_age = Some(
            v.parse()
                .map_err(|_| anyhow::anyhow!("Invalid HSTS_MAX_AGE"))?,
        );
    }
    if let Ok(v) = std::env::var("HSTS_INCLUDE_SUBDOMAINS") {
        hsts_include_subdomains = Some(v.to_lowercase() == "true");
    }

    // PROXY_ROUTES env var overrides/additional routes
    if let Ok(json_str) = std::env::var("PROXY_ROUTES") {
        match serde_json::from_str::<HashMap<String, String>>(&json_str) {
            Ok(routes) => {
                for (prefix, upstream) in routes {
                    proxy_routes.insert(prefix, upstream);
                }
            }
            Err(e) => {
                tracing::warn!("Failed to parse PROXY_ROUTES: {}", e);
            }
        }
    }

    // --- Validate required fields ---
    let domain = domain.ok_or_else(|| {
        anyhow::anyhow!("DOMAIN is required (config.toml [server].domain or env var)")
    })?;
    let email = email.ok_or_else(|| {
        anyhow::anyhow!("EMAIL is required (config.toml [server].email or env var)")
    })?;

    // --- Apply defaults ---
    let host = host.unwrap_or_else(|| "0.0.0.0".into());
    let port = port.unwrap_or(443);
    let cert_dir = cert_dir.unwrap_or_else(|| "certs/".into());
    let use_staging = use_staging.unwrap_or(true);
    let hsts_max_age = hsts_max_age.unwrap_or(31536000);
    let hsts_include_subdomains = hsts_include_subdomains.unwrap_or(true);

    // Build the HSTS header value once
    let hsts_value = if hsts_max_age > 0 {
        let mut val = format!("max-age={}", hsts_max_age);
        if hsts_include_subdomains {
            val.push_str("; includeSubDomains");
        }
        val
    } else {
        String::new() // HSTS disabled
    };

    info!(
        "Config: domain={}, host={}:{}, staging={}, hsts={}",
        domain,
        host,
        port,
        use_staging,
        if hsts_value.is_empty() {
            "off"
        } else {
            &hsts_value
        }
    );

    Ok(ResolvedConfig {
        domain,
        email,
        host,
        port,
        cert_dir,
        use_staging,
        hsts_max_age,
        hsts_value,
        proxy_routes,
    })
}
