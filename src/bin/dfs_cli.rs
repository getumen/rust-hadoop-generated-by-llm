use clap::{Parser, Subcommand};
use rust_hadoop::dfs::chunk_server_service_client::ChunkServerServiceClient;
use rust_hadoop::dfs::master_service_client::MasterServiceClient;
use rust_hadoop::dfs::{
    AllocateBlockRequest, CreateFileRequest, GetFileInfoRequest, ListFilesRequest, ReadBlockRequest,
    WriteBlockRequest,
};
use std::fs::File;
use std::io::{Read, Write};
use std::path::PathBuf;

#[derive(Parser)]
#[command(author, version, about, long_about = None)]
struct Cli {
    #[arg(short, long, default_value = "http://127.0.0.1:50051")]
    master: String,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    Ls,
    Put {
        source: PathBuf,
        dest: String,
    },
    Get {
        source: String,
        dest: PathBuf,
    },
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();
    let mut master_client = MasterServiceClient::connect(cli.master).await?
        .max_decoding_message_size(100 * 1024 * 1024);

    match cli.command {
        Commands::Ls => {
            let request = tonic::Request::new(ListFilesRequest {
                path: "/".to_string(),
            });
            let response = master_client.list_files(request).await?;
            for file in response.into_inner().files {
                println!("{}", file);
            }
        }
        Commands::Put { source, dest } => {
            // 1. Create file on Master
            let create_req = tonic::Request::new(CreateFileRequest {
                path: dest.clone(),
            });
            let create_resp = master_client.create_file(create_req).await?.into_inner();
            if !create_resp.success {
                eprintln!("Failed to create file: {}", create_resp.error_message);
                return Ok(());
            }

            // 2. Read local file and split into blocks (simplified: 1 block for now)
            let mut file = File::open(source)?;
            let mut buffer = Vec::new();
            file.read_to_end(&mut buffer)?;

            // 3. Allocate block
            let alloc_req = tonic::Request::new(AllocateBlockRequest {
                path: dest.clone(),
            });
            let alloc_resp = master_client.allocate_block(alloc_req).await?.into_inner();
            let block = alloc_resp.block.unwrap();
            let chunk_servers = alloc_resp.chunk_server_addresses;

            if chunk_servers.is_empty() {
                eprintln!("No chunk servers available");
                return Ok(());
            }

            println!("Replicating to {} servers: {:?}", chunk_servers.len(), chunk_servers);

            // 4. Write to first chunk server with replication pipeline
            let chunk_server_addr = format!("http://{}", chunk_servers[0]);
            let mut chunk_client = ChunkServerServiceClient::connect(chunk_server_addr).await?
                .max_decoding_message_size(100 * 1024 * 1024);
            
            // Pass the remaining servers as next_servers for replication pipeline
            let next_servers = chunk_servers[1..].to_vec();
            
            let write_req = tonic::Request::new(WriteBlockRequest {
                block_id: block.block_id,
                data: buffer, // Sending whole file as one block for simplicity
                next_servers, // Replication pipeline
            });
            
            let write_resp = chunk_client.write_block(write_req).await?.into_inner();
            if !write_resp.success {
                eprintln!("Failed to write block: {}", write_resp.error_message);
            } else {
                println!("File uploaded successfully with replication");
            }
        }
        Commands::Get { source, dest } => {
            // 1. Get file info from Master
            let info_req = tonic::Request::new(GetFileInfoRequest {
                path: source.clone(),
            });
            let info_resp = master_client.get_file_info(info_req).await?.into_inner();
            
            if !info_resp.found {
                eprintln!("File not found");
                return Ok(());
            }
            
            let metadata = info_resp.metadata.unwrap();
            let mut file = File::create(dest)?;

            // 2. Read blocks from ChunkServers
            for block in metadata.blocks {
                if block.locations.is_empty() {
                    eprintln!("Block {} has no locations", block.block_id);
                    continue;
                }

                // Try locations until successful
                let mut success = false;
                for location in block.locations {
                    let chunk_server_addr = format!("http://{}", location);
                    match ChunkServerServiceClient::connect(chunk_server_addr).await {
                        Ok(client) => {
                            let mut chunk_client = client.max_decoding_message_size(100 * 1024 * 1024);
                            let read_req = tonic::Request::new(ReadBlockRequest {
                                block_id: block.block_id.clone(),
                            });
                            match chunk_client.read_block(read_req).await {
                                Ok(response) => {
                                    let data = response.into_inner().data;
                                    file.write_all(&data)?;
                                    success = true;
                                    break;
                                }
                                Err(e) => eprintln!("Failed to read block from {}: {}", location, e),
                            }
                        }
                        Err(e) => eprintln!("Failed to connect to {}: {}", location, e),
                    }
                }
                
                if !success {
                    eprintln!("Failed to read block {}", block.block_id);
                    return Ok(());
                }
            }
            println!("File downloaded successfully");
        }
    }

    Ok(())
}
