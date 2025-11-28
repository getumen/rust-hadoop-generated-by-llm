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
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    let addr = args.addr.parse()?;
    let chunk_server = MyChunkServer::new(args.storage_dir);

    // Register with Master
    // We need to spawn this or do it before starting the server.
    // Since we need to listen for incoming connections, we should start the server.
    // But we also need to tell the Master we exist.
    // Let's spawn a task to register.
    
    let master_addr = format!("http://{}", args.master_addr);
    let my_addr = args.addr.clone();
    
    tokio::spawn(async move {
        // Wait a bit for the server to start (optional, but good practice if we were checking our own health)
        // Actually, we just need to connect to the master.
        
        // Retry loop could be added here
        if let Ok(mut client) = MasterServiceClient::connect(master_addr).await {
             let request = tonic::Request::new(RegisterChunkServerRequest {
                address: my_addr,
                capacity: 1024 * 1024 * 1024, // 1GB dummy capacity
            });
            
            match client.register_chunk_server(request).await {
                Ok(_) => println!("Registered with Master"),
                Err(e) => eprintln!("Failed to register with Master: {}", e),
            }
        } else {
            eprintln!("Failed to connect to Master");
        }
    });

    println!("ChunkServer listening on {}", addr);

    Server::builder()
        .add_service(ChunkServerServiceServer::new(chunk_server))
        .serve(addr)
        .await?;

    Ok(())
}
