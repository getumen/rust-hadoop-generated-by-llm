use crate::client::Client;
use std::fs::File;
use std::io::Write;
use std::path::Path;
use tokio::time::{sleep, Duration};
use uuid::Uuid;

const MASTER_ADDRS: &[&str] = &["http://127.0.0.1:50051", "http://127.0.0.1:50061"];

#[tokio::test]
async fn test_client_integration() -> Result<(), Box<dyn std::error::Error>> {
    // Wait for cluster to be ready (optional, assumes cluster is running via docker-compose)
    // In a CI env, we'd start it here. For now, assume user ran `start_sharded_cluster.sh`.

    let masters: Vec<String> = MASTER_ADDRS.iter().map(|s| s.to_string()).collect();
    let client = Client::new(masters);

    // Register host aliases for Docker -> Localhost mapping
    // Shard 1
    client.add_host_alias("http://dfs-master1-shard1:50051", "http://127.0.0.1:50051");
    // Shard 2 (mapped to port 50061 on host)
    client.add_host_alias("http://dfs-master1-shard2:50051", "http://127.0.0.1:50061");

    // ChunkServers
    client.add_host_alias(
        "http://dfs-chunkserver1-shard1:50052",
        "http://127.0.0.1:50052",
    );
    client.add_host_alias(
        "http://dfs-chunkserver1-shard2:50052",
        "http://127.0.0.1:50062",
    );

    // 1. Create a dummy file
    let source_path = Path::new("client_test_dummy.txt");
    let mut file = File::create(source_path)?;
    file.write_all(b"Hello from Rust Client Integration Test!")?;

    let uuid = Uuid::new_v4().to_string();
    let remote_path = format!("/client_test/hello_{}.txt", uuid);
    let remote_path_str = remote_path.as_str();

    // 2. Upload file
    println!("Uploading file to {}...", remote_path_str);
    client.create_file(source_path, remote_path_str).await?;

    // 3. List files
    println!("Listing files...");
    let files = client.list_files("/").await?; // Root listing might be shard-specific depending on implementation
    println!("Files found: {:?}", files);
    // Note: Due to sharding, "/" might list files only on the shard we happened to hit if Ls isn't aggregated.
    // But our test file should exist on SOME shard.

    // 4. Download file
    let dest_path = Path::new("client_test_downloaded.txt");
    println!("Downloading file...");
    client.get_file(remote_path_str, dest_path).await?;

    // 5. Verify content
    let content = std::fs::read_to_string(dest_path)?;
    assert_eq!(content, "Hello from Rust Client Integration Test!");

    // 6. Cross-shard Rename (Simulation)
    // We need to pick a source and dest that map to DIFFERENT shards.
    // Based on default hashing (hash(path) % 100), we can't easily predict without the hash function.
    // But we can try a few paths.
    let rename_dest = format!("/client_test/moved_hello_{}.txt", uuid);
    let rename_dest_str = rename_dest.as_str();
    println!("Renaming file to {}...", rename_dest_str);
    client.rename_file(remote_path_str, rename_dest_str).await?;

    // 7. Verify rename
    // Old file should be gone
    let old_res = client.get_file(remote_path_str, dest_path).await;
    assert!(old_res.is_err());

    // New file should exist
    client.get_file(rename_dest_str, dest_path).await?;

    // Cleanup
    std::fs::remove_file(source_path)?;
    std::fs::remove_file(dest_path)?;

    println!("Client Integration Test Passed!");
    Ok(())
}
