mod api;
mod auth;
mod dash;
mod fmp4;
mod oauth;
mod plugin;

use anyhow::Result;
use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

/// tidalsrc — unofficial TIDAL source plugin.
#[derive(Parser, Debug)]
#[command(name = "tidalsrc", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Run as a source plugin: JSON-RPC on stdin/stdout, logs on stderr.
    /// Started by the host player, not by hand.
    Plugin(PluginArgs),
}

#[derive(clap::Args, Debug)]
struct PluginArgs {
    /// API root, holding `/v1` and `/v2` (tests point it at a mock server).
    #[arg(long, hide = true)]
    api_base: Option<String>,
    /// OAuth server root (`/v1/oauth2/…`).
    #[arg(long, hide = true)]
    auth_base: Option<String>,
    /// Browser sign-in page root (`/authorize`).
    #[arg(long, hide = true)]
    login_base: Option<String>,
    /// Client of the device-code sign-in, for when TIDAL revokes the
    /// built-in one.
    #[arg(long, hide = true, env = "TIDALSRC_CLIENT_ID")]
    client_id: Option<String>,
    #[arg(long, hide = true, env = "TIDALSRC_CLIENT_SECRET", hide_env_values = true)]
    client_secret: Option<String>,
    /// Client of the browser (PKCE) sign-in.
    #[arg(long, hide = true, env = "TIDALSRC_PKCE_CLIENT_ID")]
    pkce_client_id: Option<String>,
    #[arg(long, hide = true, env = "TIDALSRC_PKCE_CLIENT_SECRET", hide_env_values = true)]
    pkce_client_secret: Option<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let Command::Plugin(args) = Cli::parse().command;
    // stdout carries the protocol; the host copies stderr to its log.
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();
    let mut config = api::Config::default();
    if let Some(base) = args.api_base {
        config.api = base.trim_end_matches('/').to_string();
    }
    if let Some(base) = args.auth_base {
        config.auth = base.trim_end_matches('/').to_string();
    }
    if let Some(base) = args.login_base {
        config.login = base.trim_end_matches('/').to_string();
    }
    let pick = |over: Option<String>, client: &mut String| {
        if let Some(v) = over.filter(|v| !v.trim().is_empty()) {
            *client = v.trim().to_string();
        }
    };
    pick(args.client_id, &mut config.device.id);
    pick(args.client_secret, &mut config.device.secret);
    pick(args.pkce_client_id, &mut config.pkce.id);
    pick(args.pkce_client_secret, &mut config.pkce.secret);
    plugin::run(config).await
}
