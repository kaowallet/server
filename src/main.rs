use kao_proxy::{config, routes, upstream};

use axum::extract::DefaultBodyLimit;
use axum::routing::{get, post};
use axum::Router;
use std::sync::Arc;
use tokio::net::TcpListener;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .init();

    let cfg = config::Config::from_env()?;

    let client = reqwest::Client::builder()
        .user_agent("kao-proxy/0.1")
        .https_only(true)
        .timeout(cfg.upstream_timeout)
        .connect_timeout(cfg.connect_timeout)
        .build()?;

    let drpc = upstream::Drpc::new(client, cfg.drpc_base.clone(), cfg.key);
    let state = Arc::new(routes::AppState {
        drpc,
        chains: cfg.chains,
        method_denylist: cfg.method_denylist,
    });

    let app = Router::new()
        .route("/healthz", get(routes::health))
        .route("/rpc/{chain}", post(routes::rpc_handler))
        .route(
            "/v1/{chain}/balances/{address}",
            get(routes::balances_handler),
        )
        .route(
            "/v1/{chain}/history/{address}",
            get(routes::history_handler),
        )
        .layer(DefaultBodyLimit::max(cfg.body_limit))
        .with_state(state);

    let listener = TcpListener::bind(cfg.bind).await?;
    tracing::info!(bind = %cfg.bind, "kao-proxy up (no request logging; secrets never logged)");
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}

async fn shutdown_signal() {
    use tokio::signal;
    let ctrl_c = async {
        let _ = signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let term = async {
        if let Ok(mut s) = signal::unix::signal(signal::unix::SignalKind::terminate()) {
            s.recv().await;
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = term => {},
    }
}
