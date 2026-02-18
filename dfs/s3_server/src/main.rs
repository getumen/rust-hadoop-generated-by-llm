#[cfg(test)]
mod handler_tests;
mod handlers;
mod s3_types;
mod state;

use crate::state::AppState as S3AppState;
use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{any, get},
    Router,
};
use dfs_client::Client;
use prometheus::{Encoder, IntCounterVec, Registry, TextEncoder};
use std::net::SocketAddr;
use std::sync::LazyLock;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

// Prometheus metrics for S3 server
pub static S3_REQUESTS: LazyLock<IntCounterVec> = LazyLock::new(|| {
    IntCounterVec::new(
        prometheus::opts!("s3_requests_total", "Total number of S3 requests"),
        &["method", "path", "status"],
    )
    .unwrap()
});

// Custom error type for Axum
struct InternalError;

impl IntoResponse for InternalError {
    fn into_response(self) -> Response {
        (StatusCode::INTERNAL_SERVER_ERROR, "Internal server error").into_response()
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "s3_server=debug,tower_http=debug".into()),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();

    let master_addr =
        std::env::var("MASTER_ADDR").unwrap_or_else(|_| "http://127.0.0.1:8081".to_string());
    let config_servers_env = std::env::var("CONFIG_SERVERS").unwrap_or_default();
    let config_servers: Vec<String> = config_servers_env
        .split(',')
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect();

    tracing::info!(
        "Connecting to Master at {}, Config Servers: {:?}",
        master_addr,
        config_servers
    );

    // The Client::new expects (master_addrs, config_server_addrs)
    let client = Client::new(vec![master_addr], config_servers);

    // Load shard map if config is provided (optional, Config Server is preferred)
    let shard_config_path = std::env::var("SHARD_CONFIG").ok();
    if let Some(path) = shard_config_path {
        tracing::info!("Loading shard config from {}", path);
        let shard_map = dfs_common::sharding::load_shard_map_from_config(Some(&path), 100);
        client.set_shard_map(shard_map);
    }

    let state = S3AppState { client };

    let app = Router::new()
        .route("/health", get(handle_health))
        .route("/metrics", get(handle_metrics))
        .route("/", any(handlers::handle_root))
        .route("/{*path}", any(handlers::handle_request))
        .with_state(state)
        .layer(tower_http::trace::TraceLayer::new_for_http());

    let port = std::env::var("PORT")
        .ok()
        .and_then(|s| s.parse::<u16>().ok())
        .unwrap_or(9000);
    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    tracing::info!("S3 Server listening on {}", addr);
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}

async fn handle_health() -> impl IntoResponse {
    (StatusCode::OK, "OK")
}

async fn handle_metrics() -> Result<String, InternalError> {
    let registry = Registry::new();
    registry.register(Box::new(S3_REQUESTS.clone())).unwrap();

    let mut buffer = vec![];
    let encoder = TextEncoder::new();
    encoder.encode(&registry.gather(), &mut buffer).unwrap();

    Ok(String::from_utf8(buffer).unwrap())
}
