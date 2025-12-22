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
        std::env::var("MASTER_ADDR").unwrap_or_else(|_| "http://127.0.0.1:8080".to_string());

    tracing::info!("Connecting to Master at {}", master_addr);

    // The Client::new expects Vec<String>
    let client = Client::new(vec![master_addr]);

    // Load shard map if config is provided
    let shard_config_path = std::env::var("SHARD_CONFIG").ok();
    if let Some(path) = shard_config_path {
        tracing::info!("Loading shard config from {}", path);
        // Default virtual nodes = 100
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
