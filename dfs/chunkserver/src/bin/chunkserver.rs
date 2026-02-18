use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
    Router,
};
use clap::Parser;
use dfs_chunkserver::chunkserver::MyChunkServer;
use dfs_chunkserver::dfs::chunk_server_service_server::ChunkServerServiceServer;
use dfs_chunkserver::dfs::master_service_client::MasterServiceClient;
use prometheus::{Encoder, Gauge, Registry, TextEncoder};
use std::path::PathBuf;
use tonic::transport::Server;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

// Axum state for sharing the ChunkServer
#[derive(Clone)]
struct AppState {
    chunk_server: MyChunkServer,
}

// Custom error type for Axum
struct InternalError;

impl IntoResponse for InternalError {
    fn into_response(self) -> Response {
        (StatusCode::INTERNAL_SERVER_ERROR, "Internal server error").into_response()
    }
}

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    #[arg(short, long, default_value = "127.0.0.1:50052")]
    addr: String,

    #[arg(short, long, value_delimiter = ',')]
    config_servers: Vec<String>,

    #[arg(short, long, default_value = "/tmp/chunkserver_data")]
    storage_dir: PathBuf,

    /// Address to advertise to master (defaults to addr if not specified)
    #[arg(long)]
    advertise_addr: Option<String>,

    #[arg(long, default_value = "8082")]
    http_port: u16,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "chunkserver=debug,dfs_chunkserver=debug".into()),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();

    let args = Args::parse();
    let addr = args.addr.parse()?;

    let chunk_server = MyChunkServer::new(args.storage_dir.clone(), args.config_servers.clone());

    // Start HTTP Server for metrics
    let app_state = AppState {
        chunk_server: chunk_server.clone(),
    };

    let app = Router::new()
        .route("/health", get(handle_health))
        .route("/metrics", get(handle_metrics))
        .with_state(app_state);

    let http_addr: std::net::SocketAddr = ([0, 0, 0, 0], args.http_port).into();
    tokio::spawn(async move {
        tracing::info!("HTTP server listening on {}", http_addr);
        let listener = tokio::net::TcpListener::bind(http_addr).await.unwrap();
        axum::serve(listener, app).await.unwrap();
    });

    // Start background scrubber

    let server_for_scrubber = chunk_server.clone();
    tokio::spawn(async move {
        MyChunkServer::run_background_scrubber(
            server_for_scrubber,
            std::time::Duration::from_secs(60),
        )
        .await;
    });

    // Registration and Heartbeat Loop
    let my_addr = args.advertise_addr.unwrap_or_else(|| args.addr.clone());
    let storage_dir_heartbeat = args.storage_dir.clone();
    let chunk_server_heartbeat = chunk_server.clone();

    tokio::spawn(async move {
        // 1. Initial Discovery
        // Check if shard map is already loaded
        let needs_fetch = {
            let shard_map = chunk_server_heartbeat.shard_map.lock().unwrap();
            shard_map.get_all_masters().is_empty()
        };

        if needs_fetch {
            loop {
                if chunk_server_heartbeat.refresh_shard_map().await.is_ok() {
                    tracing::info!("✓ Initial shard map fetched");
                    break;
                }
                tracing::warn!("✗ Failed to fetch initial shard map. Retrying...");
                tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
            }
        } else {
            tracing::info!("✓ Shard map already loaded from config file");
        }

        // 2. Main Loop: Refresh ShardMap & Heartbeat to all Masters
        loop {
            // Periodically refresh shard map
            let _ = chunk_server_heartbeat.refresh_shard_map().await;

            // Gather stats
            let available_space = fs2::free_space(&storage_dir_heartbeat).unwrap_or(0);
            let total_space = fs2::total_space(&storage_dir_heartbeat).unwrap_or(0);
            let used_space = total_space.saturating_sub(available_space);

            // Count chunks
            let chunk_count = std::fs::read_dir(&storage_dir_heartbeat)
                .map(|read_dir| read_dir.count())
                .unwrap_or(0) as u64;

            // Identify all master leaders from ShardMap
            let masters = {
                let shard_map = chunk_server_heartbeat.shard_map.lock().unwrap();
                shard_map.get_all_masters()
            };

            for master_addr in masters {
                let master_url = if master_addr.starts_with("http://") {
                    master_addr.clone()
                } else {
                    format!("http://{}", master_addr)
                };
                tracing::debug!("Heartbeating to master: {}", master_url);
                match MasterServiceClient::connect(master_url.clone()).await {
                    Ok(mut client) => {
                        let request = tonic::Request::new(dfs_chunkserver::dfs::HeartbeatRequest {
                            chunk_server_address: my_addr.clone(),
                            used_space,
                            available_space,
                            chunk_count,
                        });

                        match client.heartbeat(request).await {
                            Ok(response) => {
                                tracing::debug!("Heartbeat successful to {}", master_url);
                                let resp = response.into_inner();
                                for command in resp.commands {
                                    if command.r#type == 1 {
                                        // REPLICATE
                                        let chunk_server_clone = chunk_server_heartbeat.clone();
                                        let block_id = command.block_id.clone();
                                        let target = command.target_chunk_server_address.clone();

                                        tokio::spawn(async move {
                                            let _ = chunk_server_clone
                                                .initiate_replication(&block_id, &target)
                                                .await;
                                        });
                                    }
                                }
                            }
                            Err(e) => {
                                tracing::warn!("Heartbeat failed to {}: {}", master_url, e);
                                // Master might not be leader or is down
                            }
                        }
                    }
                    Err(e) => {
                        tracing::warn!("Failed to connect to master {}: {}", master_url, e);
                    }
                }
            }

            tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
        }
    });

    tracing::info!("ChunkServer listening on {}", addr);

    Server::builder()
        .add_service(
            ChunkServerServiceServer::new(chunk_server)
                .max_decoding_message_size(100 * 1024 * 1024),
        )
        .serve(addr)
        .await?;

    Ok(())
}

async fn handle_health() -> impl IntoResponse {
    (StatusCode::OK, "OK")
}

async fn handle_metrics(State(app_state): State<AppState>) -> Result<String, InternalError> {
    let storage_dir = app_state.chunk_server.get_storage_dir();
    let registry = Registry::new();

    let available_space_gauge = Gauge::new(
        "dfs_chunkserver_available_space_bytes",
        "Available space on chunkserver in bytes",
    )
    .unwrap();
    let used_space_gauge = Gauge::new(
        "dfs_chunkserver_used_space_bytes",
        "Used space on chunkserver in bytes",
    )
    .unwrap();
    let chunk_count_gauge = Gauge::new(
        "dfs_chunkserver_total_chunks",
        "Total number of chunks on this chunkserver",
    )
    .unwrap();

    registry
        .register(Box::new(available_space_gauge.clone()))
        .unwrap();
    registry
        .register(Box::new(used_space_gauge.clone()))
        .unwrap();
    registry
        .register(Box::new(chunk_count_gauge.clone()))
        .unwrap();

    // Gather stats
    let available_space = fs2::free_space(&storage_dir).unwrap_or(0);
    let total_space = fs2::total_space(&storage_dir).unwrap_or(0);
    let used_space = total_space.saturating_sub(available_space);
    let chunk_count = std::fs::read_dir(&storage_dir)
        .map(|read_dir| read_dir.count())
        .unwrap_or(0) as u64;

    available_space_gauge.set(available_space as f64);
    used_space_gauge.set(used_space as f64);
    chunk_count_gauge.set(chunk_count as f64);

    let mut buffer = vec![];
    let encoder = TextEncoder::new();
    encoder.encode(&registry.gather(), &mut buffer).unwrap();

    Ok(String::from_utf8(buffer).unwrap())
}
