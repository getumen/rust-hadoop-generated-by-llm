use clap::{Parser, Subcommand};
use dfs_client::dfs::master_service_client::MasterServiceClient;
use dfs_client::dfs::*;
use dfs_client::Client;
use std::path::PathBuf;
use std::sync::Arc;
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

    #[arg(long, value_delimiter = ',')]
    config_servers: Vec<String>,

    #[arg(long, default_value_t = 5)]
    max_retries: usize,

    #[arg(long, default_value_t = 500)]
    initial_backoff_ms: u64,

    #[arg(
        long,
        help = "Map internal hostnames to local addresses (e.g. master1:50051=localhost:50051)"
    )]
    host_alias: Vec<String>,

    #[arg(long, help = "CA certificate for TLS")]
    ca_cert: Option<String>,

    #[arg(long, help = "Domain name for TLS certificate verification")]
    domain_name: Option<String>,

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
    /// Inspect file metadata (including block locations)
    Inspect {
        path: String,
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
    /// Trigger background data shuffling for a prefix
    Shuffle {
        prefix: String,
    },
    /// Run performance benchmarks
    Benchmark {
        #[command(subcommand)]
        action: BenchmarkAction,
    },
}

#[derive(Subcommand)]
pub enum BenchmarkAction {
    /// Benchmark write throughput
    Write {
        /// Number of files to create
        #[arg(short, long, default_value_t = 100)]
        count: usize,
        /// Size of each file in bytes
        #[arg(short, long, default_value_t = 1048576)]
        size: usize,
        /// Number of concurrent tasks
        #[arg(short = 'n', long, default_value_t = 10)]
        concurrency: usize,
        /// Prefix for benchmark files
        #[arg(short, long, default_value = "bench_write")]
        prefix: String,
    },
    /// Benchmark read throughput
    Read {
        /// Prefix of files to read
        #[arg(short, long, default_value = "bench_write")]
        prefix: String,
        /// Number of concurrent tasks
        #[arg(short = 'n', long, default_value_t = 10)]
        concurrency: usize,
    },
    /// Benchmark sustained write throughput (stress test)
    StressWrite {
        /// Duration of the test in seconds
        #[arg(short, long, default_value_t = 30)]
        duration: u64,
        /// Size of each file in bytes
        #[arg(short, long, default_value_t = 1048576)]
        size: usize,
        /// Number of concurrent tasks
        #[arg(short = 'n', long, default_value_t = 10)]
        concurrency: usize,
        /// Prefix for benchmark files
        #[arg(short, long, default_value = "bench_stress")]
        prefix: String,
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
    let _ = rustls::crypto::ring::default_provider().install_default();

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

    let client = Client::new(master_addrs, cli.config_servers)
        .with_retry_config(cli.max_retries, cli.initial_backoff_ms)
        .with_tls_config(cli.ca_cert.clone(), cli.domain_name.clone());

    for alias_pair in cli.host_alias {
        if let Some((alias, real)) = alias_pair.split_once('=') {
            client.add_host_alias(alias.trim(), real.trim());
        }
    }

    let client = Arc::new(client);

    async fn connect_master(
        master: &str,
        ca_cert: &Option<String>,
        domain_name: &Option<String>,
    ) -> anyhow::Result<MasterServiceClient<tonic::transport::Channel>> {
        let mut master_url = if master.starts_with("http") {
            master.to_string()
        } else {
            format!("http://{}", master)
        };

        if ca_cert.is_some() && master_url.starts_with("http://") {
            master_url = master_url.replace("http://", "https://");
        }

        let mut endpoint = tonic::transport::Endpoint::from_shared(master_url.clone())?;

        if let Some(ca_path) = ca_cert {
            let domain = domain_name.clone().unwrap_or_else(|| {
                master_url
                    .split("://")
                    .last()
                    .unwrap_or("")
                    .split(':')
                    .next()
                    .unwrap_or("localhost")
                    .to_string()
            });
            if let Ok(tls_config) = dfs_common::security::get_client_tls_config(ca_path, &domain) {
                endpoint = endpoint
                    .tls_config(tls_config)
                    .expect("Failed to apply TLS config");
            }
        }

        let channel = endpoint.connect().await?;
        Ok(MasterServiceClient::new(channel))
    }
    match cli.command {
        Commands::Ls => {
            let files = client.list_all_files().await?;
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
        Commands::Inspect { path } => {
            let metadata = client.get_file_info(&path).await?;
            if let Some(meta) = metadata {
                println!("File Metadata for: {}", meta.path);
                println!("  Size: {} bytes", meta.size);
                println!("  Blocks: {}", meta.blocks.len());
                for (i, block) in meta.blocks.iter().enumerate() {
                    println!(
                        "    Block {}: ID={}, Size={}, Locations={:?}",
                        i, block.block_id, block.size, block.locations
                    );
                }
            } else {
                println!("File not found: {}", path);
            }
        }
        Commands::Rename { source, dest } => {
            client.rename_file(&source, &dest).await?;
            println!("File renamed successfully: {} -> {}", source, dest);
        }
        Commands::SafeMode { action } => {
            let mut grpc_client =
                connect_master(&cli.master, &cli.ca_cert, &cli.domain_name).await?;

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
            let mut grpc_client =
                connect_master(&cli.master, &cli.ca_cert, &cli.domain_name).await?;

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
        Commands::Shuffle { prefix } => {
            client.initiate_shuffle(&prefix).await?;
            println!("Triggered background shuffling for prefix: {}", prefix);
        }
        Commands::Benchmark { action } => {
            match action {
                BenchmarkAction::Write {
                    count,
                    size,
                    concurrency,
                    prefix,
                } => {
                    println!(
                        "🚀 Starting Write Benchmark: {} files, {} bytes each, concurrency={}",
                        count, size, concurrency
                    );

                    let start = std::time::Instant::now();
                    let mut latencies = Vec::with_capacity(count);
                    let mut join_set = tokio::task::JoinSet::new();
                    let client = Arc::new(client);
                    let run_id = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap()
                        .as_secs();

                    // Distribute work
                    for i in 0..count {
                        let client = Arc::clone(&client);
                        let prefix = prefix.clone();
                        join_set.spawn(async move {
                            let filename = format!("{}/{}/bench_{:010}", prefix, run_id, i);
                            let data = vec![0u8; size]; // Zero data for speed
                            let op_start = std::time::Instant::now();
                            let res = client.create_file_from_buffer(data, &filename).await;
                            (res, op_start.elapsed())
                        });

                        // Limit concurrency
                        if join_set.len() >= concurrency {
                            if let Some(res) = join_set.join_next().await {
                                let (res, lat) = res?;
                                res?;
                                latencies.push(lat);
                            }
                        }
                    }

                    // Wait for remaining
                    while let Some(res) = join_set.join_next().await {
                        let (res, lat) = res?;
                        res?;
                        latencies.push(lat);
                    }

                    let total_duration = start.elapsed();
                    print_stats("Write", count, size, total_duration, latencies);
                }
                BenchmarkAction::Read {
                    prefix,
                    concurrency,
                } => {
                    println!(
                        "🚀 Starting Read Benchmark: prefix={}, concurrency={}",
                        prefix, concurrency
                    );

                    // 1. List files
                    let all_files = client.list_all_files().await?;
                    let target_files: Vec<String> = all_files
                        .into_iter()
                        .filter(|f| f.starts_with(&prefix))
                        .collect();

                    if target_files.is_empty() {
                        println!("No files found matching prefix: {}", prefix);
                        return Ok(());
                    }

                    println!("Found {} files to read", target_files.len());

                    let start = std::time::Instant::now();
                    let mut latencies = Vec::with_capacity(target_files.len());
                    let mut total_bytes = 0;
                    let mut join_set = tokio::task::JoinSet::new();
                    let client = Arc::new(client);

                    for filename in target_files {
                        let client = Arc::clone(&client);
                        join_set.spawn(async move {
                            let op_start = std::time::Instant::now();
                            let res = client.get_file_content(&filename).await;
                            (res, op_start.elapsed())
                        });

                        if join_set.len() >= concurrency {
                            if let Some(res) = join_set.join_next().await {
                                let (res, lat) = res?;
                                let data = res?;
                                total_bytes += data.len();
                                latencies.push(lat);
                            }
                        }
                    }

                    while let Some(res) = join_set.join_next().await {
                        let (res, lat) = res?;
                        let data = res?;
                        total_bytes += data.len();
                        latencies.push(lat);
                    }

                    let total_duration = start.elapsed();
                    print_stats(
                        "Read",
                        latencies.len(),
                        total_bytes / latencies.len().max(1),
                        total_duration,
                        latencies,
                    );
                }
                BenchmarkAction::StressWrite {
                    duration,
                    size,
                    concurrency,
                    prefix,
                } => {
                    println!(
                        "🔥 Starting Write Stress Test: duration={}s, size={} bytes, concurrency={}",
                        duration, size, concurrency
                    );

                    let start = std::time::Instant::now();
                    let end_time = start + std::time::Duration::from_secs(duration);
                    let mut latencies = Vec::new();
                    let mut join_set = tokio::task::JoinSet::new();
                    let client = Arc::new(client);
                    let run_id = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap()
                        .as_secs();

                    let mut total_ops = 0;
                    let mut success_count = 0;
                    let mut error_count = 0;

                    let mut last_report = std::time::Instant::now();

                    loop {
                        let now = std::time::Instant::now();
                        if now >= end_time && join_set.is_empty() {
                            break;
                        }

                        // Report progress every 5 seconds
                        if now.duration_since(last_report).as_secs() >= 5 {
                            let elapsed = start.elapsed().as_secs_f64();
                            let current_throughput =
                                (success_count as f64 * size as f64) / (1024.0 * 1024.0) / elapsed;
                            println!(
                                "⏱️  Progress: {}s/{}s, Success: {}, Errors: {}, Current Throughput: {:.2} MB/s",
                                elapsed.round(),
                                duration,
                                success_count,
                                error_count,
                                current_throughput
                            );
                            last_report = now;
                        }

                        // Spawn new tasks if under concurrency limit and time remains
                        while join_set.len() < concurrency && std::time::Instant::now() < end_time {
                            let client = Arc::clone(&client);
                            let filename =
                                format!("{}/{}/stress_{:010}", prefix, run_id, total_ops);
                            let data = vec![0u8; size];
                            join_set.spawn(async move {
                                let op_start = std::time::Instant::now();
                                let res = client.create_file_from_buffer(data, &filename).await;
                                (res, op_start.elapsed())
                            });
                            total_ops += 1;
                        }

                        // Collect results
                        tokio::select! {
                            res = join_set.join_next(), if !join_set.is_empty() => {
                                if let Some(res) = res {
                                    match res {
                                        Ok((Ok(_), lat)) => {
                                            success_count += 1;
                                            latencies.push(lat);
                                        }
                                        Ok((Err(e), _)) => {
                                            error_count += 1;
                                            tracing::error!("Stress test operation failed: {}", e);
                                        }
                                        Err(e) => {
                                            error_count += 1;
                                            tracing::error!("Task Join Error: {}", e);
                                        }
                                    }
                                }
                            }
                            _ = tokio::time::sleep(std::time::Duration::from_millis(10)), if join_set.is_empty() && std::time::Instant::now() < end_time => {
                                // Just wait a bit to avoid busy loop if throttled
                            }
                            else => {
                                if std::time::Instant::now() >= end_time && join_set.is_empty() {
                                    break;
                                }
                            }
                        }
                    }

                    let total_duration = start.elapsed();
                    println!("\n🚨 Stress Test Completed!");
                    println!("Total Success: {}", success_count);
                    println!("Total Errors:  {}", error_count);
                    print_stats(
                        "Stress Write",
                        success_count,
                        size,
                        total_duration,
                        latencies,
                    );
                }
            }
        }
    }

    Ok(())
}

fn print_stats(
    name: &str,
    count: usize,
    avg_size: usize,
    total_duration: std::time::Duration,
    mut latencies: Vec<std::time::Duration>,
) {
    let total_size_mb = (count as f64 * avg_size as f64) / (1024.0 * 1024.0);
    let throughput = total_size_mb / total_duration.as_secs_f64();
    let ops_per_sec = count as f64 / total_duration.as_secs_f64();

    latencies.sort();
    let min = latencies.first().cloned().unwrap_or_default();
    let max = latencies.last().cloned().unwrap_or_default();
    let avg = if latencies.is_empty() {
        std::time::Duration::default()
    } else {
        latencies.iter().sum::<std::time::Duration>() / latencies.len() as u32
    };
    let p95 = latencies
        .get(latencies.len() * 95 / 100)
        .cloned()
        .unwrap_or_default();
    let p99 = latencies
        .get(latencies.len() * 99 / 100)
        .cloned()
        .unwrap_or_default();

    println!("\n📊 {} Benchmark Results:", name);
    println!("----------------------------------------");
    println!("Total Operations:  {}", count);
    println!("Total Time:        {:.2?}", total_duration);
    println!("Throughput:        {:.2} MB/s", throughput);
    println!("Throughput (OPS):  {:.2} ops/s", ops_per_sec);
    println!();
    println!("Latency Statistics:");
    println!("  Min:  {:.2?}", min);
    println!("  Avg:  {:.2?}", avg);
    println!("  P95:  {:.2?}", p95);
    println!("  P99:  {:.2?}", p99);
    println!("  Max:  {:.2?}", max);
    println!("----------------------------------------");
}
