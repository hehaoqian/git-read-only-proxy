use anyhow::{Context, Result};
use clap::Parser;
use std::{net::SocketAddr, path::PathBuf};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};
use url::Url;

#[derive(Parser, Debug)]
#[command(name = "git-read-only-proxy")]
#[command(version)]
#[command(
    about = "A read-only reverse proxy for the git HTTPS smart-HTTP protocol",
    long_about = "Forwards git fetch/clone requests to an upstream server while \
                  blocking all write operations (git push / git-receive-pack).\n\n\
                  Run without --cert / --key to listen on plain HTTP (useful for \
                  local development or when TLS is terminated upstream)."
)]
struct Args {
    /// Upstream git server base URL (e.g. https://github.com or http://localhost:8080).
    #[arg(short, long)]
    upstream: String,

    /// TCP port to listen on.
    #[arg(short, long, default_value = "443")]
    port: u16,

    /// IP address / hostname to bind to.
    #[arg(long, default_value = "127.0.0.1")]
    hostname: String,

    /// Path to a PEM-encoded TLS certificate file (enables HTTPS).
    #[arg(long)]
    cert: Option<PathBuf>,

    /// Path to a PEM-encoded TLS private key file (enables HTTPS).
    #[arg(long)]
    key: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "git_read_only_proxy=info,warn".into()),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();

    let args = Args::parse();

    let upstream = Url::parse(&args.upstream).context("Invalid upstream URL")?;

    let addr: SocketAddr = format!("{}:{}", args.hostname, args.port)
        .parse()
        .context("Invalid bind address")?;

    let config = git_read_only_proxy::Config {
        upstream,
        addr,
        cert: args.cert,
        key: args.key,
    };

    git_read_only_proxy::run(config).await
}
