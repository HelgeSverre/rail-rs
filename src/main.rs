use std::time::Duration;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use rail::{Config, balancer::Strategy, store::Store, trace::trace, upstream::Credentials};
use tokio::net::TcpListener;
use tracing::info;
use tracing_subscriber::EnvFilter;
use url::Url;

/// RAIL: Redundant Array of Inexpensive Links.
#[derive(Debug, Parser)]
#[command(version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run a RAIL instance.
    Serve(Box<ServeArgs>),
    /// Follow a short URL hop by hop and report how close it is to the browser redirect limit.
    Trace { url: String },
}

#[derive(Debug, clap::Args)]
struct ServeArgs {
    /// Instance name, reported in the X-Rail-Instance header.
    #[arg(long, env = "RAIL_NAME", default_value = "rail")]
    name: String,
    #[arg(long, env = "RAIL_LISTEN", default_value = "0.0.0.0:8080")]
    listen: String,
    /// Base URL this instance is reachable at. Short URLs are minted under it.
    #[arg(long, env = "RAIL_PUBLIC_URL", default_value = "http://localhost:8080")]
    public_url: Url,
    /// Comma-separated upstreams (see the README for the list), rail+<url>, yourls+<url>, or "none".
    #[arg(
        long = "upstream",
        env = "RAIL_UPSTREAMS",
        value_delimiter = ',',
        default_value = "is.gd,tinyurl,da.gd,spoo.me"
    )]
    upstreams: Vec<String>,
    #[arg(long, env = "RAIL_STRATEGY", value_enum, default_value_t)]
    strategy: Strategy,
    /// SQLite database path.
    #[arg(long, env = "RAIL_DB", default_value = "rail.db")]
    db: String,
    /// Seconds between health sweeps. Each nested layer sweeps independently, so be polite.
    #[arg(long, env = "RAIL_HEALTH_INTERVAL", default_value_t = 300)]
    health_interval: u64,
    /// Accept targets on loopback, private and link-local addresses (intranet deployments).
    #[arg(long, env = "RAIL_ALLOW_PRIVATE_TARGETS")]
    allow_private_targets: bool,
    /// API token for the tinyurl-api upstream.
    #[arg(long, env = "RAIL_TINYURL_TOKEN", hide_env_values = true)]
    tinyurl_token: Option<String>,
    /// Secret signature token for yourls+<url> upstreams.
    #[arg(long, env = "RAIL_YOURLS_SIGNATURE", hide_env_values = true)]
    yourls_signature: Option<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    match Cli::parse().command {
        Command::Serve(args) => serve(*args).await,
        Command::Trace { url } => {
            let client = rail::http_client()?;
            let t = trace(&client, &url).await?;
            for (i, hop) in t.hops.iter().enumerate() {
                let status = hop
                    .status
                    .map_or_else(|| "ERR".to_owned(), |s| s.to_string());
                let via = hop
                    .instance
                    .as_deref()
                    .map(|n| format!("  [rail:{n}]"))
                    .unwrap_or_default();
                println!(
                    "{i:>3}  {status}  {:>5}ms  {}{via}",
                    hop.latency_ms, hop.url
                );
            }
            println!(
                "\n{} redirects, headroom {} before ERR_TOO_MANY_REDIRECTS{}",
                t.redirects,
                t.headroom,
                if t.gave_up {
                    " (a browser would have given up)"
                } else {
                    ""
                }
            );
            Ok(())
        }
    }
}

async fn serve(args: ServeArgs) -> Result<()> {
    let credentials = Credentials {
        tinyurl_token: args.tinyurl_token.filter(|t| !t.is_empty()),
        yourls_signature: args.yourls_signature.filter(|s| !s.is_empty()),
    };
    let upstreams =
        rail::upstream::parse_all(args.upstreams.iter().map(String::as_str), &credentials)?;
    let config = Config {
        name: args.name,
        public_url: args.public_url,
        upstreams,
        strategy: args.strategy,
        health_interval: Duration::from_secs(args.health_interval.max(1)),
        allow_private_targets: args.allow_private_targets,
        credentials,
    };
    let store = Store::open(&args.db)?;
    let listener = TcpListener::bind(&args.listen)
        .await
        .with_context(|| format!("binding {}", args.listen))?;
    info!(
        name = config.name,
        public_url = %config.public_url,
        upstreams = ?config.upstreams.iter().map(ToString::to_string).collect::<Vec<_>>(),
        strategy = ?config.strategy,
        "RAIL online"
    );
    rail::serve(config, store, listener).await
}
