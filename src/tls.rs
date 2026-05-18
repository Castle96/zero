use std::fs;
use std::path::Path;
use std::sync::Arc;

use rustls::pki_types::{CertificateDer, PrivatePkcs8KeyDer};
use rustls::ServerConfig;
use tokio_rustls::TlsAcceptor;
use tracing::{info, warn};

use crate::acme::LetsEncryptConfig;

pub struct TlsConfig {
    certs: Vec<CertificateDer<'static>>,
    key: PrivatePkcs8KeyDer<'static>,
}

impl TlsConfig {
    pub async fn load_from_le_config(
        le_config: &LetsEncryptConfig,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        if let Some(tls) = Self::load_from_cert_dir(&le_config.cert_dir) {
            return Ok(tls);
        }

        let common_paths = [
            std::path::PathBuf::from("/etc/letsencrypt/live")
                .join(&le_config.domain)
                .join("fullchain.pem"),
            std::path::PathBuf::from("/etc/letsencrypt/live")
                .join(&le_config.domain)
                .join("cert.pem"),
        ];
        for cert_path in &common_paths {
            let key_path = cert_path.parent().unwrap_or(std::path::Path::new("")).join(
                if cert_path.file_name() == Some("fullchain.pem".as_ref()) {
                    "privkey.pem"
                } else {
                    "key.pem"
                },
            );
            if cert_path.exists() && key_path.exists() {
                info!(
                    "Loading existing Let's Encrypt certificates from {}",
                    cert_path.display()
                );
                return Self::load_from_files(cert_path, &key_path);
            }
        }

        if let Some(tls) = Self::load_from_env() {
            return Ok(tls);
        }

        info!("No existing Let's Encrypt certificates found; provisioning now");
        let metadata = le_config.provision_certificate().await?;
        Self::load_from_files(&metadata.cert_path, &metadata.key_path)
    }

    fn load_from_cert_dir(cert_dir: &Path) -> Option<Self> {
        let cert_path = cert_dir.join("cert.pem");
        let key_path = cert_dir.join("key.pem");

        if cert_path.exists() && key_path.exists() {
            info!(
                "Loading existing Let's Encrypt certificates from {}",
                cert_dir.display()
            );
            return Self::load_from_files(&cert_path, &key_path).ok();
        }
        None
    }

    pub fn load_from_env() -> Option<Self> {
        let cert_path = std::env::var("TLS_CERT_PATH").ok()?;
        let key_path = std::env::var("TLS_KEY_PATH").ok()?;
        info!(
            "Loading TLS certificates from env vars: {}, {}",
            cert_path, key_path
        );
        match Self::load_from_files(cert_path.as_ref(), key_path.as_ref()) {
            Ok(tls) => Some(tls),
            Err(e) => {
                warn!("Failed to load TLS cert from env vars: {}", e);
                None
            }
        }
    }

    pub fn load_from_files(
        cert_path: &Path,
        key_path: &Path,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let cert_pem = fs::read(cert_path)?;
        let mut cert_reader = std::io::Cursor::new(&cert_pem);

        let certs: Vec<CertificateDer<'static>> = rustls_pemfile::certs(&mut cert_reader)
            .filter_map(|r| r.ok())
            .collect();

        if certs.is_empty() {
            return Err("No certificates found in cert file".into());
        }

        let key_pem = fs::read(key_path)?;
        let mut key_reader = std::io::Cursor::new(&key_pem);

        let mut keys: Vec<PrivatePkcs8KeyDer<'static>> =
            rustls_pemfile::pkcs8_private_keys(&mut key_reader)
                .filter_map(|r| r.ok())
                .collect();

        if keys.is_empty() {
            return Err("No private keys found in key file".into());
        }

        let key = keys.remove(0);

        Ok(Self { certs, key })
    }

    pub fn create_acceptor(&self) -> Result<TlsAcceptor, Box<dyn std::error::Error + Send + Sync>> {
        let config = ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(
                self.certs.clone(),
                rustls::pki_types::PrivateKeyDer::Pkcs8(self.key.clone_key()),
            )?;

        Ok(TlsAcceptor::from(Arc::new(config)))
    }
}
