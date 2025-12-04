use clap::{Parser, Subcommand};
use rust_hadoop::dfs::chunk_server_service_client::ChunkServerServiceClient;
use rust_hadoop::dfs::master_service_client::MasterServiceClient;
use rust_hadoop::dfs::{
    AllocateBlockRequest, CreateFileRequest, GetFileInfoRequest, ListFilesRequest,
    ReadBlockRequest, RenameRequest, WriteBlockRequest,
};
use std::fs::File;
use std::io::{Read, Write};
use std::path::PathBuf;

use tokio::time::{sleep, Duration};

#[derive(Parser)]
#[command(
    author,
    version,
    about,
    long_about = "Rust Hadoop DFS CLI\n\nAutomatically discovers the Leader Master node and retries operations on failure."
)]
struct Cli {
    #[arg(short, long, default_value = "http://127.0.0.1:50051")]
    master: String,

    #[arg(long, default_value_t = 5)]
    max_retries: usize,

    #[arg(long, default_value_t = 500)]
    initial_backoff_ms: u64,

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
    /// Rename a file (supports cross-shard rename)
    Rename {
        /// Source file path
        source: String,
        /// Destination file path
        dest: String,
    },
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // ... (main body remains same until execute_with_retry definition)
    let cli = Cli::parse();

    let master_addrs: Vec<String> = cli
        .master
        .split(',')
        .map(|s| s.trim().to_string())
        .collect();

    let max_retries = cli.max_retries;
    let initial_backoff_ms = cli.initial_backoff_ms;

    match cli.command {
        Commands::Ls => {
            let response = execute_with_retry(
                &master_addrs,
                max_retries,
                initial_backoff_ms,
                |mut client| async move {
                    let request = tonic::Request::new(ListFilesRequest {
                        path: "/".to_string(),
                    });
                    client.list_files(request).await
                },
            )
            .await?;

            for file in response.into_inner().files {
                println!("{}", file);
            }
        }
        Commands::Put { source, dest } => {
            // 1. Create file on Master
            let create_resp = execute_with_retry(
                &master_addrs,
                max_retries,
                initial_backoff_ms,
                |mut client| {
                    let dest = dest.clone();
                    async move {
                        let create_req = tonic::Request::new(CreateFileRequest { path: dest });
                        let response = client.create_file(create_req).await?;
                        let inner = response.get_ref();
                        if !inner.success && inner.error_message == "Not Leader" {
                            return Err(tonic::Status::unavailable(format!(
                                "Not Leader|{}",
                                inner.leader_hint
                            )));
                        }
                        Ok(response)
                    }
                },
            )
            .await?
            .into_inner();

            if !create_resp.success {
                eprintln!("Failed to create file: {}", create_resp.error_message);
                return Ok(());
            }

            // 2. Read local file and split into blocks (simplified: 1 block for now)
            let mut file = File::open(source)?;
            let mut buffer = Vec::new();
            file.read_to_end(&mut buffer)?;

            // 3. Allocate block
            let alloc_resp = execute_with_retry(
                &master_addrs,
                max_retries,
                initial_backoff_ms,
                |mut client| {
                    let dest = dest.clone();
                    async move {
                        let alloc_req = tonic::Request::new(AllocateBlockRequest { path: dest });
                        let response = client.allocate_block(alloc_req).await?;
                        let inner = response.get_ref();
                        if inner.block.is_none() {
                            return Err(tonic::Status::unavailable(format!(
                                "Not Leader|{}",
                                inner.leader_hint
                            )));
                        }
                        Ok(response)
                    }
                },
            )
            .await?
            .into_inner();

            let block = alloc_resp.block.unwrap();
            let chunk_servers = alloc_resp.chunk_server_addresses;

            if chunk_servers.is_empty() {
                eprintln!("No chunk servers available");
                return Ok(());
            }

            println!(
                "Replicating to {} servers: {:?}",
                chunk_servers.len(),
                chunk_servers
            );

            // 4. Write to first chunk server with replication pipeline
            let chunk_server_addr = format!("http://{}", chunk_servers[0]);
            let mut chunk_client = ChunkServerServiceClient::connect(chunk_server_addr)
                .await?
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
            let info_resp = execute_with_retry(
                &master_addrs,
                max_retries,
                initial_backoff_ms,
                |mut client| {
                    let source = source.clone();
                    async move {
                        let info_req = tonic::Request::new(GetFileInfoRequest { path: source });
                        client.get_file_info(info_req).await
                    }
                },
            )
            .await?
            .into_inner();

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
                            let mut chunk_client =
                                client.max_decoding_message_size(100 * 1024 * 1024);
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
                                Err(e) => {
                                    eprintln!("Failed to read block from {}: {}", location, e)
                                }
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
        Commands::Rename { source, dest } => {
            let rename_resp = execute_with_retry(
                &master_addrs,
                max_retries,
                initial_backoff_ms,
                |mut client| {
                    let source = source.clone();
                    let dest = dest.clone();
                    async move {
                        let rename_req = tonic::Request::new(RenameRequest {
                            source_path: source,
                            dest_path: dest,
                        });
                        let response = client.rename(rename_req).await?;
                        let inner = response.get_ref();
                        if !inner.success && inner.error_message == "Not Leader" {
                            return Err(tonic::Status::unavailable(format!(
                                "Not Leader|{}",
                                inner.leader_hint
                            )));
                        }
                        if !inner.redirect_hint.is_empty() {
                            return Err(tonic::Status::out_of_range(format!(
                                "REDIRECT:{}",
                                inner.redirect_hint
                            )));
                        }
                        Ok(response)
                    }
                },
            )
            .await?
            .into_inner();

            if rename_resp.success {
                println!("File renamed successfully: {} -> {}", source, dest);
            } else {
                eprintln!("Failed to rename file: {}", rename_resp.error_message);
            }
        }
    }

    Ok(())
}

async fn execute_with_retry<F, Fut, T>(
    masters: &[String],
    max_retries: usize,
    initial_backoff_ms: u64,
    f: F,
) -> Result<T, Box<dyn std::error::Error>>
where
    F: Fn(MasterServiceClient<tonic::transport::Channel>) -> Fut,
    Fut: std::future::Future<Output = Result<T, tonic::Status>>,
{
    let mut attempt = 0;
    let mut backoff = Duration::from_millis(initial_backoff_ms);
    let mut leader_hint: Option<String> = None;

    loop {
        attempt += 1;

        let targets = if let Some(hint) = leader_hint.take() {
            // Ensure hint has http:// prefix
            let hint_with_prefix = if hint.starts_with("http://") {
                hint
            } else {
                format!("http://{}", hint)
            };
            eprintln!("Using leader hint with prefix: {}", hint_with_prefix);
            let mut t = vec![hint_with_prefix];
            // Add original masters as fallback, avoiding duplicates if possible, but simple append is fine for now
            t.extend_from_slice(masters);
            t
        } else {
            masters.to_vec()
        };

        for master_addr in targets {
            if master_addr.is_empty() {
                continue;
            }

            let client = match MasterServiceClient::connect(master_addr.clone()).await {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("Failed to connect to {}: {}", master_addr, e);
                    continue;
                }
            };
            let client = client.max_decoding_message_size(100 * 1024 * 1024);

            match f(client).await {
                Ok(res) => return Ok(res),
                Err(status) => {
                    let msg = status.message();
                    if msg.starts_with("REDIRECT:") {
                        // Use splitn to split only on the first colon
                        let parts: Vec<&str> = msg.splitn(2, ':').collect();
                        if parts.len() > 1 && !parts[1].is_empty() {
                            leader_hint = Some(parts[1].to_string());
                            eprintln!("Received SHARD REDIRECT to: {}", parts[1]);
                            break; // Break inner loop to retry immediately with hint
                        }
                    }

                    if msg.starts_with("Not Leader|") {
                        let parts: Vec<&str> = msg.split('|').collect();
                        if parts.len() > 1 && !parts[1].is_empty() {
                            leader_hint = Some(parts[1].to_string());
                            eprintln!("Received leader hint: {}", parts[1]);
                            break; // Break inner loop to retry immediately with hint
                        }
                    }

                    if msg.contains("Not Leader") || status.code() == tonic::Code::Unavailable {
                        continue;
                    }
                    return Err(Box::new(status));
                }
            }
        }

        if attempt >= max_retries {
            break;
        }

        // If we have a hint, we might want to skip backoff or reduce it?
        // For now, keep backoff to avoid hot loop if hint is wrong.
        if leader_hint.is_some() {
            eprintln!("Retrying with leader hint...");
        } else {
            eprintln!("No leader found, retrying in {:?}...", backoff);
            sleep(backoff).await;
            backoff = std::cmp::min(backoff * 2, Duration::from_secs(5));
        }
    }

    Err("No available leader found after retries".into())
}
