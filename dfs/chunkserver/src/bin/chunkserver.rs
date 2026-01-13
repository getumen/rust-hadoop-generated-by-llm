use clap::Parser;
use dfs_chunkserver::chunkserver::MyChunkServer;
use dfs_chunkserver::dfs::chunk_server_service_server::ChunkServerServiceServer;
use dfs_chunkserver::dfs::master_service_client::MasterServiceClient;
use dfs_chunkserver::dfs::RegisterChunkServerRequest;
use std::path::PathBuf;
use tonic::transport::Server;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    #[arg(short, long, default_value = "127.0.0.1:50052")]
    addr: String,

    #[arg(short, long, default_value = "127.0.0.1:50051")]
    master_addr: String,

    #[arg(short, long, default_value = "/tmp/chunkserver_data")]
    storage_dir: PathBuf,

    /// Address to advertise to master (defaults to addr if not specified)
    #[arg(long)]
    advertise_addr: Option<String>,
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

    let master_addrs_raw: Vec<String> =
        args.master_addr.split(',').map(|s| s.to_string()).collect();

    let chunk_server = MyChunkServer::new(args.storage_dir.clone(), master_addrs_raw.clone());

    // Start background scrubber
    let storage_dir_scrubber = args.storage_dir.clone();
    let master_addrs_for_scrubber = master_addrs_raw.clone();
    tokio::spawn(async move {
        // Run scrubber every 60 seconds
        MyChunkServer::run_background_scrubber(
            storage_dir_scrubber,
            master_addrs_for_scrubber,
            std::time::Duration::from_secs(60),
        )
        .await;
    });

    // Register with Master
    let master_addrs: Vec<String> = args
        .master_addr
        .split(',')
        .map(|s| format!("http://{}", s))
        .collect();
    let my_addr = args.advertise_addr.unwrap_or_else(|| args.addr.clone());
    let storage_dir_heartbeat = args.storage_dir.clone();
    let chunk_server_heartbeat = chunk_server.clone();

    tokio::spawn(async move {
        // 1. Initial Registration
        loop {
            let mut registered = false;
            for master_addr in &master_addrs {
                match MasterServiceClient::connect(master_addr.clone()).await {
                    Ok(mut client) => {
                        let request = tonic::Request::new(RegisterChunkServerRequest {
                            address: my_addr.clone(),
                            capacity: 1024 * 1024 * 1024, // 1GB dummy capacity
                        });

                        match client.register_chunk_server(request).await {
                            Ok(_) => {
                                tracing::info!("✓ Registered with Master at {}", master_addr);
                                registered = true;
                                // We try to register with all masters initially
                            }
                            Err(e) => {
                                tracing::error!(
                                    "Failed to register with Master {}: {}",
                                    master_addr,
                                    e
                                )
                            }
                        }
                    }
                    Err(e) => {
                        tracing::error!("Failed to connect to Master {}: {}", master_addr, e);
                    }
                }
            }

            if registered {
                break;
            } else {
                tracing::warn!("✗ Failed to register with any Master. Retrying...");
                tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
            }
        }

        // 2. Heartbeat Loop
        println!("Starting heartbeat loop...");
        loop {
            // Gather stats
            let available_space = fs2::free_space(&storage_dir_heartbeat).unwrap_or(0);
            let total_space = fs2::total_space(&storage_dir_heartbeat).unwrap_or(0);
            let used_space = total_space.saturating_sub(available_space);

            // Count chunks (files in storage dir)
            let chunk_count = std::fs::read_dir(&storage_dir_heartbeat)
                .map(|read_dir| read_dir.count())
                .unwrap_or(0) as u64;

            for master_addr in &master_addrs {
                match MasterServiceClient::connect(master_addr.clone()).await {
                    Ok(mut client) => {
                        let request = tonic::Request::new(dfs_chunkserver::dfs::HeartbeatRequest {
                            chunk_server_address: my_addr.clone(),
                            used_space,
                            available_space,
                            chunk_count,
                        });

                        match client.heartbeat(request).await {
                            Ok(response) => {
                                let resp = response.into_inner();
                                for command in resp.commands {
                                    if command.r#type == 1 {
                                        // REPLICATE
                                        println!(
                                            "Received replication command for block {} to {}",
                                            command.block_id, command.target_chunk_server_address
                                        );
                                        let chunk_server_clone = chunk_server_heartbeat.clone();
                                        let block_id = command.block_id.clone();
                                        let target = command.target_chunk_server_address.clone();

                                        tokio::spawn(async move {
                                            if let Err(e) = chunk_server_clone
                                                .initiate_replication(&block_id, &target)
                                                .await
                                            {
                                                eprintln!("Replication failed: {}", e);
                                            } else {
                                                println!(
                                                    "Replication of block {} to {} completed",
                                                    block_id, target
                                                );
                                            }
                                        });
                                    }
                                }
                            }
                            Err(e) => {
                                eprintln!("Heartbeat failed to {}: {}", master_addr, e);
                            }
                        }
                    }
                    Err(_) => {
                        // Silent failure for connection error (master might be down)
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
