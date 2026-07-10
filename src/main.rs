//! Binary entrypoint: initialize tracing, load config, build the router, serve.

use std::path::Path;
use std::process::ExitCode;

use claude_multi_proxy::config::Config;
use claude_multi_proxy::{build_state, log_key_presence, proxy};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> ExitCode {
    // `RUST_LOG` overrides; default to `info`.
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .init();

    let config_path = Config::resolve_path();
    let config = match Config::load(Path::new(&config_path)) {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("{e}");
            return ExitCode::FAILURE;
        }
    };

    let bind_addr = format!("{}:{}", config.server.host, config.server.port);
    log_key_presence(&config);

    let state = match build_state(config) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!("failed to build HTTP client: {e}");
            return ExitCode::FAILURE;
        }
    };

    let app = proxy::router(state);

    let listener = match tokio::net::TcpListener::bind(&bind_addr).await {
        Ok(l) => l,
        Err(e) => {
            tracing::error!("failed to bind {bind_addr}: {e}");
            return ExitCode::FAILURE;
        }
    };

    tracing::info!("proxy listening on http://{bind_addr}");
    if let Err(e) = axum::serve(listener, app).await {
        tracing::error!("server error: {e}");
        return ExitCode::FAILURE;
    }

    ExitCode::SUCCESS
}
