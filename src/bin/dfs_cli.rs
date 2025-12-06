use clap::{Parser, Subcommand};
use rust_hadoop::client::Client;
use std::path::PathBuf;

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
    let cli = Cli::parse();

    let master_addrs: Vec<String> = cli
        .master
        .split(',')
        .map(|s| s.trim().to_string())
        .collect();

    let client = Client::new(master_addrs);

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
    }

    Ok(())
}
