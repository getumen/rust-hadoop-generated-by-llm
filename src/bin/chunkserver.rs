use clap::Parser;
use rust_hadoop::chunkserver::MyChunkServer;
use rust_hadoop::dfs::chunk_server_service_server::ChunkServerServiceServer;
use rust_hadoop::dfs::master_service_client::MasterServiceClient;
use rust_hadoop::dfs::RegisterChunkServerRequest;
use std::path::PathBuf;
use tonic::transport::Server;

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
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    let addr = args.addr.parse()?;

    let master_addrs_raw: Vec<String> =
        args.master_addr.split(',').map(|s| s.to_string()).collect();

    let chunk_server = MyChunkServer::new(args.storage_dir.clone(), master_addrs_raw.clone());

    // Start background scrubber
    let storage_dir = args.storage_dir.clone();
    let master_addrs_for_scrubber = master_addrs_raw.clone();
    tokio::spawn(async move {
        // Run scrubber every 60 seconds
        MyChunkServer::run_background_scrubber(
            storage_dir,
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

    tokio::spawn(async move {
        // Retry loop for master registration
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
                                println!("✓ Registered with Master at {}", master_addr);
                                registered = true;
                                break; // Connected to one master, good for now.
                                       // In a real system, we might need to register with the active one.
                                       // Since only active master accepts connections (others are waiting on lock),
                                       // this works naturally.
                            }
                            Err(e) => {
                                eprintln!("Failed to register with Master {}: {}", master_addr, e)
                            }
                        }
                    }
                    Err(e) => {
                        eprintln!("Failed to connect to Master {}: {}", master_addr, e);
                    }
                }
            }

            if registered {
                // Keep checking or re-registering periodically?
                // For now, just sleep and re-register to ensure we stay connected if master fails over
                tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
            } else {
                eprintln!("✗ Failed to register with any Master. Retrying...");
                tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
            }
        }
    });

    println!("ChunkServer listening on {}", addr);

    Server::builder()
        .add_service(
            ChunkServerServiceServer::new(chunk_server)
                .max_decoding_message_size(100 * 1024 * 1024),
        )
        .serve(addr)
        .await?;

    Ok(())
}
