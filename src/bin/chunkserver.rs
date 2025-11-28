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
    let chunk_server = MyChunkServer::new(args.storage_dir);

    // Register with Master
    let master_addr = format!("http://{}", args.master_addr);
    let my_addr = args.advertise_addr.unwrap_or_else(|| args.addr.clone());
    
    tokio::spawn(async move {
        // Retry loop for master registration
        for attempt in 1..=10 {
            tokio::time::sleep(tokio::time::Duration::from_secs(attempt)).await;
            
            match MasterServiceClient::connect(master_addr.clone()).await {
                Ok(mut client) => {
                    let request = tonic::Request::new(RegisterChunkServerRequest {
                        address: my_addr.clone(),
                        capacity: 1024 * 1024 * 1024, // 1GB dummy capacity
                    });
                    
                    match client.register_chunk_server(request).await {
                        Ok(_) => {
                            println!("✓ Registered with Master at {}", master_addr);
                            return;
                        }
                        Err(e) => eprintln!("Attempt {}/10: Failed to register with Master: {}", attempt, e),
                    }
                }
                Err(e) => {
                    eprintln!("Attempt {}/10: Failed to connect to Master: {}", attempt, e);
                }
            }
        }
        eprintln!("✗ Failed to register with Master after 10 attempts");
    });

    println!("ChunkServer listening on {}", addr);

    Server::builder()
        .add_service(
            ChunkServerServiceServer::new(chunk_server)
                .max_decoding_message_size(100 * 1024 * 1024)
        )
        .serve(addr)
        .await?;

    Ok(())
}
