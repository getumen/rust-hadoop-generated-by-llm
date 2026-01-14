#[cfg(test)]
mod handler_tests;
mod handlers;
mod s3_types;
mod state;

use crate::state::AppState as S3AppState;
use axum::{routing::any, Router};
use dfs_client::Client;
use std::net::SocketAddr;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

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
        let shard_map = dfs_client::sharding::load_shard_map_from_config(Some(&path), 100);
        client.set_shard_map(shard_map);
    }

    let state = S3AppState { client };

    let app = Router::new()
        .route("/", any(handlers::handle_root))
        .route("/{*path}", any(handlers::handle_request))
        .with_state(state)
        .layer(tower_http::trace::TraceLayer::new_for_http());

    let addr = SocketAddr::from(([0, 0, 0, 0], 9000));
    tracing::info!("S3 Server listening on {}", addr);
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}
