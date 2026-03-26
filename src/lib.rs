pub mod filter;
pub mod proxy;

pub use proxy::create_router;

use anyhow::{Context, Result};
use std::{net::SocketAddr, path::PathBuf, sync::Arc};
use url::Url;

/// Runtime configuration for the proxy server.
#[derive(Debug, Clone)]
pub struct Config {
    /// Base URL of the upstream git server (e.g. `https://github.com`).
    pub upstream: Url,
    /// Socket address the proxy should listen on.
    pub addr: SocketAddr,
    /// Optional path to a PEM-encoded TLS certificate (enables HTTPS).
    pub cert: Option<PathBuf>,
    /// Optional path to a PEM-encoded TLS private key (enables HTTPS).
    pub key: Option<PathBuf>,
}

/// Start the proxy server with the given configuration.
///
/// Listens on HTTP when `cert`/`key` are absent, or HTTPS when both are
/// provided.
pub async fn run(config: Config) -> Result<()> {
    let app = create_router(config.upstream);

    match (config.cert, config.key) {
        (Some(cert), Some(key)) => start_https(app, config.addr, &cert, &key).await,
        (None, None) => {
            tracing::info!("Starting HTTP proxy server on {}", config.addr);
            let listener = tokio::net::TcpListener::bind(config.addr)
                .await
                .context("Failed to bind TCP listener")?;
            axum::serve(listener, app)
                .await
                .context("HTTP server error")?;
            Ok(())
        }
        _ => anyhow::bail!(
            "Both --cert and --key must be provided together to enable HTTPS, \
             or omit both to use plain HTTP"
        ),
    }
}

/// Start an HTTPS server using the provided PEM certificate and key.
async fn start_https(
    app: axum::Router,
    addr: SocketAddr,
    cert_path: &std::path::Path,
    key_path: &std::path::Path,
) -> Result<()> {
    use hyper::body::Incoming;
    use hyper_util::rt::{TokioExecutor, TokioIo};
    use hyper_util::server::conn::auto::Builder as AutoBuilder;
    use rustls::ServerConfig;
    use tokio_rustls::TlsAcceptor;
    use tower::ServiceExt;

    let cert_data =
        std::fs::read(cert_path).with_context(|| format!("reading cert {:?}", cert_path))?;
    let key_data =
        std::fs::read(key_path).with_context(|| format!("reading key {:?}", key_path))?;

    let certs = rustls_pemfile::certs(&mut cert_data.as_ref())
        .collect::<Result<Vec<_>, _>>()
        .context("Failed to parse TLS certificate")?;

    let key = rustls_pemfile::private_key(&mut key_data.as_ref())
        .context("Failed to parse TLS private key")?
        .ok_or_else(|| anyhow::anyhow!("No private key found in {:?}", key_path))?;

    let tls_config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .context("Failed to build TLS server config")?;

    let acceptor = TlsAcceptor::from(Arc::new(tls_config));
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .context("Failed to bind TCP listener")?;

    tracing::info!("Starting HTTPS proxy server on {}", addr);

    loop {
        let (tcp_stream, peer) = listener.accept().await?;
        let acceptor = acceptor.clone();
        let app = app.clone();

        tokio::spawn(async move {
            let tls_stream = match acceptor.accept(tcp_stream).await {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!("TLS handshake failed from {peer}: {e}");
                    return;
                }
            };

            let io = TokioIo::new(tls_stream);
            let svc = hyper::service::service_fn(move |req: hyper::Request<Incoming>| {
                app.clone().oneshot(req.map(axum::body::Body::new))
            });

            if let Err(e) = AutoBuilder::new(TokioExecutor::new())
                .serve_connection(io, svc)
                .await
            {
                tracing::debug!("HTTPS connection error from {peer}: {e}");
            }
        });
    }
}
