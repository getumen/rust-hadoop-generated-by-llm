use clap::{Parser, Subcommand};
use dfs_client::Client;
use std::path::PathBuf;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

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
    /// Get or set Safe Mode status
    SafeMode {
        #[command(subcommand)]
        action: SafeModeAction,
    },
    /// Raft cluster management
    Cluster {
        #[command(subcommand)]
        action: ClusterAction,
    },
}

#[derive(Subcommand)]
enum SafeModeAction {
    /// Get current Safe Mode status
    Get,
    /// Enter Safe Mode (block writes)
    Enter,
    /// Leave Safe Mode (allow writes)
    Leave,
}

#[derive(Subcommand)]
enum ClusterAction {
    /// Get cluster information
    Info,
    /// Add a server to the Raft cluster
    AddServer {
        /// Server ID to add
        server_id: u32,
        /// Server HTTP address for Raft RPC
        server_address: String,
    },
    /// Remove a server from the Raft cluster
    RemoveServer {
        /// Server ID to remove
        server_id: u32,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "dfs_cli=info,dfs_client=info".into()),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();

    let cli = Cli::parse();

    let master_addrs: Vec<String> = cli
        .master
        .split(',')
        .map(|s| s.trim().to_string())
        .collect();

    let client =
        Client::new(master_addrs).with_retry_config(cli.max_retries, cli.initial_backoff_ms);

    match cli.command {
        Commands::Ls => {
            let files = client.list_files("/").await?;
            for file in files {
                println!("{}", file);
            }
        }
        Commands::Put { source, dest } => {
            client.create_file(&source, &dest).await?;
            println!("File uploaded successfully with replication");
        }
        Commands::Get { source, dest } => {
            client.get_file(&source, &dest).await?;
            println!("File downloaded successfully");
        }
        Commands::Rename { source, dest } => {
            client.rename_file(&source, &dest).await?;
            println!("File renamed successfully: {} -> {}", source, dest);
        }
        Commands::SafeMode { action } => {
            use dfs_client::dfs::master_service_client::MasterServiceClient;
            use dfs_client::dfs::{GetSafeModeStatusRequest, SetSafeModeRequest};

            let master_addr = if cli.master.starts_with("http://") {
                cli.master.clone()
            } else {
                format!("http://{}", cli.master)
            };

            let mut grpc_client = MasterServiceClient::connect(master_addr).await?;

            match action {
                SafeModeAction::Get => {
                    let response = grpc_client
                        .get_safe_mode_status(GetSafeModeStatusRequest {})
                        .await?
                        .into_inner();

                    println!("Safe Mode Status:");
                    println!("  Active: {}", response.is_safe_mode);
                    println!("  Manual: {}", response.is_manual);
                    println!("  ChunkServers: {}", response.chunk_server_count);
                    println!(
                        "  Blocks: {}/{}",
                        response.reported_blocks, response.expected_blocks
                    );
                    println!("  Threshold: {}%", (response.threshold * 100.0) as u32);
                }
                SafeModeAction::Enter => {
                    let response = grpc_client
                        .set_safe_mode(SetSafeModeRequest { enter: true })
                        .await?
                        .into_inner();

                    if response.success {
                        println!("Entered Safe Mode");
                    } else {
                        println!("Failed to enter Safe Mode: {}", response.error_message);
                    }
                }
                SafeModeAction::Leave => {
                    let response = grpc_client
                        .set_safe_mode(SetSafeModeRequest { enter: false })
                        .await?
                        .into_inner();

                    if response.success {
                        println!("Left Safe Mode");
                    } else {
                        println!("Failed to leave Safe Mode: {}", response.error_message);
                    }
                }
            }
        }
        Commands::Cluster { action } => {
            use dfs_client::dfs::master_service_client::MasterServiceClient;
            use dfs_client::dfs::{
                AddRaftServerRequest, GetClusterInfoRequest, RemoveRaftServerRequest,
            };

            let master_addr = if cli.master.starts_with("http://") {
                cli.master.clone()
            } else {
                format!("http://{}", cli.master)
            };

            let mut grpc_client = MasterServiceClient::connect(master_addr).await?;

            match action {
                ClusterAction::Info => {
                    let response = grpc_client
                        .get_cluster_info(GetClusterInfoRequest {})
                        .await?
                        .into_inner();

                    println!("Raft Cluster Info:");
                    println!("  Node ID: {}", response.node_id);
                    println!("  Role: {}", response.role);
                    println!("  Term: {}", response.current_term);
                    println!("  Leader ID: {}", response.leader_id);
                    println!("  Leader Address: {}", response.leader_address);
                    println!("  Commit Index: {}", response.commit_index);
                    println!("  Last Applied: {}", response.last_applied);
                    println!("  Members ({}):", response.members.len());
                    for member in response.members {
                        println!(
                            "    - [{}] {} {}",
                            member.server_id,
                            member.address,
                            if member.is_self { "(self)" } else { "" }
                        );
                    }
                }
                ClusterAction::AddServer {
                    server_id,
                    server_address,
                } => {
                    let response = grpc_client
                        .add_raft_server(AddRaftServerRequest {
                            server_id,
                            server_address: server_address.clone(),
                        })
                        .await?
                        .into_inner();

                    if response.success {
                        println!("Added server {} ({}) to cluster", server_id, server_address);
                    } else {
                        println!("Failed to add server: {}", response.error_message);
                        if !response.leader_hint.is_empty() {
                            println!("Leader hint: {}", response.leader_hint);
                        }
                    }
                }
                ClusterAction::RemoveServer { server_id } => {
                    let response = grpc_client
                        .remove_raft_server(RemoveRaftServerRequest { server_id })
                        .await?
                        .into_inner();

                    if response.success {
                        println!("Removed server {} from cluster", server_id);
                    } else {
                        println!("Failed to remove server: {}", response.error_message);
                        if !response.leader_hint.is_empty() {
                            println!("Leader hint: {}", response.leader_hint);
                        }
                    }
                }
            }
        }
    }

    Ok(())
}
