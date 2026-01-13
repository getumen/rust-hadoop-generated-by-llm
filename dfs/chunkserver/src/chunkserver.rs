use crate::dfs::chunk_server_service_server::ChunkServerService;
use crate::dfs::{ReadBlockRequest, ReadBlockResponse, WriteBlockRequest, WriteBlockResponse};
use std::fs;
use std::io::{Read, Write};
use std::path::PathBuf;
use tonic::{Request, Response, Status};
use tracing::Instrument;

#[derive(Debug, Clone)]
pub struct MyChunkServer {
    storage_dir: PathBuf,
    master_addrs: Vec<String>,
}

impl MyChunkServer {
    pub fn new(storage_dir: PathBuf, master_addrs: Vec<String>) -> Self {
        fs::create_dir_all(&storage_dir).expect("Failed to create storage directory");
        MyChunkServer {
            storage_dir,
            master_addrs,
        }
    }

    fn calculate_checksums(data: &[u8]) -> Vec<u32> {
        let mut checksums = Vec::new();
        for chunk in data.chunks(512) {
            let mut hasher = crc32fast::Hasher::new();
            hasher.update(chunk);
            checksums.push(hasher.finalize());
        }
        checksums
    }

    fn write_block_local(&self, block_id: &str, data: &[u8]) -> Result<(), std::io::Error> {
        let path = self.storage_dir.join(block_id);
        let meta_path = self.storage_dir.join(format!("{}.meta", block_id));

        // Write data
        let mut file = fs::File::create(&path)?;
        file.write_all(data)?;

        // Calculate and write checksums
        let checksums = Self::calculate_checksums(data);
        let mut meta_file = fs::File::create(&meta_path)?;
        for checksum in checksums {
            meta_file.write_all(&checksum.to_be_bytes())?;
        }

        Ok(())
    }

    fn verify_block(&self, block_id: &str, data: &[u8]) -> Result<(), String> {
        let meta_path = self.storage_dir.join(format!("{}.meta", block_id));
        if !meta_path.exists() {
            // If meta file is missing, we can't verify.
            // For now, treat as error to enforce integrity.
            return Err("Checksum file missing".to_string());
        }

        let mut meta_file = fs::File::open(&meta_path).map_err(|e| e.to_string())?;
        let mut meta_data = Vec::new();
        meta_file
            .read_to_end(&mut meta_data)
            .map_err(|e| e.to_string())?;

        let expected_checksums: Vec<u32> = meta_data
            .chunks_exact(4)
            .map(|chunk| {
                let bytes: [u8; 4] = chunk
                    .try_into()
                    .map_err(|_| "Invalid checksum size".to_string())?;
                Ok(u32::from_be_bytes(bytes))
            })
            .collect::<Result<Vec<u32>, String>>()?;

        let actual_checksums = Self::calculate_checksums(data);

        if expected_checksums.len() != actual_checksums.len() {
            return Err("Checksum count mismatch".to_string());
        }

        for (i, (expected, actual)) in expected_checksums
            .iter()
            .zip(actual_checksums.iter())
            .enumerate()
        {
            if expected != actual {
                return Err(format!("Checksum mismatch at chunk {}", i));
            }
        }

        Ok(())
    }

    async fn recover_block(&self, block_id: &str) -> Result<(), String> {
        tracing::info!(
            "Attempting to recover block {} from healthy replica",
            block_id
        );

        // 1. Query Master for block locations
        let mut locations = Vec::new();
        for master_addr in &self.master_addrs {
            match crate::dfs::master_service_client::MasterServiceClient::connect(format!(
                "http://{}",
                master_addr
            ))
            .await
            {
                Ok(mut client) => {
                    let request = tonic::Request::new(crate::dfs::GetBlockLocationsRequest {
                        block_id: block_id.to_string(),
                    });

                    match client.get_block_locations(request).await {
                        Ok(response) => {
                            let resp = response.into_inner();
                            if resp.found {
                                locations = resp.locations;
                                break;
                            }
                        }
                        Err(e) => {
                            tracing::error!(
                                "Failed to get block locations from {}: {}",
                                master_addr,
                                e
                            );
                        }
                    }
                }
                Err(e) => {
                    tracing::error!("Failed to connect to master {}: {}", master_addr, e);
                }
            }
        }

        if locations.is_empty() {
            return Err("No replica locations found for block".to_string());
        }

        // 2. Try to fetch from each replica (excluding ourselves)
        let my_addr = std::env::var("CHUNK_SERVER_ADDR").unwrap_or_default();
        for location in locations {
            if location.contains(&my_addr) {
                continue; // Skip ourselves
            }

            tracing::info!("Trying to fetch block {} from {}", block_id, location);

            match crate::dfs::chunk_server_service_client::ChunkServerServiceClient::connect(
                format!("http://{}", location),
            )
            .await
            {
                Ok(mut client) => {
                    let request = tonic::Request::new(crate::dfs::ReadBlockRequest {
                        block_id: block_id.to_string(),
                    });

                    match client.read_block(request).await {
                        Ok(response) => {
                            let data = response.into_inner().data;

                            // 3. Verify the fetched data
                            if self.verify_block(block_id, &data).is_ok() {
                                // 4. Replace corrupted block
                                if let Err(e) = self.write_block_local(block_id, &data) {
                                    tracing::error!("Failed to write recovered block: {}", e);
                                    continue;
                                }

                                tracing::info!(
                                    "Successfully recovered block {} from {}",
                                    block_id,
                                    location
                                );
                                return Ok(());
                            } else {
                                tracing::error!(
                                    "Fetched block from {} is also corrupted",
                                    location
                                );
                            }
                        }
                        Err(e) => {
                            tracing::error!("Failed to read block from {}: {}", location, e);
                        }
                    }
                }
                Err(e) => {
                    tracing::error!("Failed to connect to {}: {}", location, e);
                }
            }
        }

        Err("Failed to recover block from any replica".to_string())
    }

    pub async fn initiate_replication(
        &self,
        block_id: &str,
        target_addr: &str,
    ) -> Result<(), String> {
        // 1. Read block data locally
        let path = self.storage_dir.join(block_id);
        let data = match fs::read(&path) {
            Ok(d) => d,
            Err(e) => return Err(format!("Failed to read block {}: {}", block_id, e)),
        };

        // 2. Send ReplicateBlock RPC to target
        let target_url = format!("http://{}", target_addr);
        let mut client =
            match crate::dfs::chunk_server_service_client::ChunkServerServiceClient::connect(
                target_url,
            )
            .await
            {
                Ok(c) => c,
                Err(e) => {
                    return Err(format!(
                        "Failed to connect to target {}: {}",
                        target_addr, e
                    ))
                }
            };

        let request = tonic::Request::new(crate::dfs::ReplicateBlockRequest {
            block_id: block_id.to_string(),
            data,
            next_servers: vec![], // No further forwarding
        });

        match client.replicate_block(request).await {
            Ok(_) => Ok(()),
            Err(e) => Err(format!("Replication failed: {}", e)),
        }
    }

    pub async fn run_background_scrubber(
        storage_dir: PathBuf,
        master_addrs: Vec<String>,
        interval: std::time::Duration,
    ) {
        let server = MyChunkServer {
            storage_dir: storage_dir.clone(),
            master_addrs,
        };

        loop {
            tokio::time::sleep(interval).await;
            tracing::info!("Starting background block scrubber...");

            let server_clone = server.clone();
            let storage_dir_clone = storage_dir.clone();

            let result = tokio::task::spawn_blocking(move || {
                // We need a runtime handle to execute async recover_block from within spawn_blocking
                // However, recover_block is async and spawn_blocking expects sync closure.
                // It's better to collect corrupted blocks here and recover them outside the blocking task,
                // or use a different approach.
                // For simplicity in this architecture, let's just identify corrupted blocks here.
                let mut corrupted_blocks = Vec::new();

                match fs::read_dir(&storage_dir_clone) {
                    Ok(entries) => {
                        for entry in entries.flatten() {
                            let path = entry.path();
                            // Skip meta files and directories
                            if path.is_dir() || path.extension().is_some_and(|ext| ext == "meta") {
                                continue;
                            }

                            if let Some(block_id) = path.file_name().and_then(|n| n.to_str()) {
                                // Read block data
                                match fs::read(&path) {
                                    Ok(data) => match server_clone.verify_block(block_id, &data) {
                                        Ok(_) => {}
                                        Err(_e) => {
                                            tracing::error!(
                                                "Corruption detected in block {} by scrubber!",
                                                block_id
                                            );
                                            corrupted_blocks.push(block_id.to_string());
                                        }
                                    },
                                    Err(_e) => {
                                        tracing::error!("Failed to read block {}: {}", block_id, _e)
                                    }
                                }
                            }
                        }
                    }
                    Err(e) => tracing::error!("Failed to read storage directory: {}", e),
                }
                corrupted_blocks
            })
            .await;

            match result {
                Ok(corrupted_blocks) => {
                    for block_id in corrupted_blocks {
                        tracing::info!("Attempting background recovery for block {}", block_id);
                        if let Err(e) = server.recover_block(&block_id).await {
                            tracing::error!(
                                "Background recovery failed for block {}: {}",
                                block_id,
                                e
                            );
                        }
                    }
                }
                Err(e) => tracing::error!("Scrubber task failed: {}", e),
            }

            tracing::info!("Background block scrubber finished.");
        }
    }
}

#[tonic::async_trait]
impl ChunkServerService for MyChunkServer {
    async fn write_block(
        &self,
        request: Request<WriteBlockRequest>,
    ) -> Result<Response<WriteBlockResponse>, Status> {
        let request_id = dfs_common::telemetry::get_request_id(&request);
        let span = tracing::info_span!("write_block", request_id = %request_id);
        async move {
            let req = request.into_inner();
            // ...

            // Write block locally with checksums
            if let Err(e) = self.write_block_local(&req.block_id, &req.data) {
                return Ok(Response::new(WriteBlockResponse {
                    success: false,
                    error_message: e.to_string(),
                }));
            }

            // If there are next servers in the pipeline, replicate to them
            if !req.next_servers.is_empty() {
                let next_server = &req.next_servers[0];
                let remaining_servers = req.next_servers[1..].to_vec();

                // Forward to next server in pipeline
                let next_addr = format!("http://{}", next_server);
                match tonic::transport::Endpoint::from_shared(next_addr.clone()) {
                    Ok(endpoint) => match endpoint.connect().await {
                        Ok(channel) => {
                            let mut client = crate::dfs::chunk_server_service_client::ChunkServerServiceClient::with_interceptor(channel, dfs_common::telemetry::propagation_interceptor(request_id.clone()))
                                .max_decoding_message_size(100 * 1024 * 1024);
                            let replicate_req = crate::dfs::ReplicateBlockRequest {
                                block_id: req.block_id,
                                data: req.data,
                                next_servers: remaining_servers,
                            };

                            if let Err(e) = client.replicate_block(replicate_req).await {
                                tracing::error!("Failed to replicate to {}: {}", next_server, e);
                                // Continue even if replication fails
                            }
                        }
                        Err(e) => {
                            tracing::error!(
                                "Failed to connect to {} for replication: {}",
                                next_server,
                                e
                            );
                        }
                    },
                    Err(e) => tracing::error!("Invalid URL {}: {}", next_addr, e),
                }
            }

            Ok(Response::new(WriteBlockResponse {
                success: true,
                error_message: "".to_string(),
            }))
        }
        .instrument(span)
        .await
    }

    async fn read_block(
        &self,
        request: Request<ReadBlockRequest>,
    ) -> Result<Response<ReadBlockResponse>, Status> {
        let request_id = dfs_common::telemetry::get_request_id(&request);
        let span = tracing::info_span!("read_block", request_id = %request_id);
        async move {
            let req = request.into_inner();
            let path = self.storage_dir.join(&req.block_id);

            match fs::File::open(&path) {
                Ok(mut file) => {
                    let mut data = Vec::new();
                    if let Err(e) = file.read_to_end(&mut data) {
                        return Err(Status::internal(e.to_string()));
                    }

                    // Verify checksums
                    if let Err(e) = self.verify_block(&req.block_id, &data) {
                        tracing::error!(
                            "CRITICAL: Data corruption detected for block {}: {}",
                            req.block_id,
                            e
                        );

                        // Attempt automatic recovery
                        tracing::warn!("Attempting automatic recovery for block {}", req.block_id);
                        match self.recover_block(&req.block_id).await {
                            Ok(_) => {
                                tracing::info!(
                                    "Block {} successfully recovered, retrying read",
                                    req.block_id
                                );
                                // Re-read the recovered block
                                let mut file = fs::File::open(&path)
                                    .map_err(|e| Status::internal(e.to_string()))?;
                                let mut recovered_data = Vec::new();
                                file.read_to_end(&mut recovered_data)
                                    .map_err(|e| Status::internal(e.to_string()))?;

                                // Verify recovered data
                                if let Err(e) = self.verify_block(&req.block_id, &recovered_data) {
                                    return Err(Status::data_loss(format!(
                                        "Recovered block is still corrupted: {}",
                                        e
                                    )));
                                }

                                return Ok(Response::new(ReadBlockResponse {
                                    data: recovered_data,
                                }));
                            }
                            Err(recovery_err) => {
                                tracing::error!(
                                    "Failed to recover block {}: {}",
                                    req.block_id,
                                    recovery_err
                                );
                                return Err(Status::data_loss(format!(
                                    "Data corruption detected: {}. Recovery failed: {}",
                                    e, recovery_err
                                )));
                            }
                        }
                    }

                    Ok(Response::new(ReadBlockResponse { data }))
                }
                Err(_) => Err(Status::not_found("Block not found")),
            }
        }
        .instrument(span)
        .await
    }

    async fn replicate_block(
        &self,
        request: Request<crate::dfs::ReplicateBlockRequest>,
    ) -> Result<Response<crate::dfs::ReplicateBlockResponse>, Status> {
        let request_id = dfs_common::telemetry::get_request_id(&request);
        let span = tracing::info_span!("replicate_block", request_id = %request_id);
        async move {
            let req = request.into_inner();

            // Write block locally with checksums
            if let Err(e) = self.write_block_local(&req.block_id, &req.data) {
                return Ok(Response::new(crate::dfs::ReplicateBlockResponse {
                    success: false,
                    error_message: e.to_string(),
                }));
            }

            if !req.next_servers.is_empty() {
                let next_server = &req.next_servers[0];
                let remaining_servers = req.next_servers[1..].to_vec();

                let next_addr = format!("http://{}", next_server);
                match tonic::transport::Endpoint::from_shared(next_addr.clone()) {
                    Ok(endpoint) => match endpoint.connect().await {
                        Ok(channel) => {
                            let mut client = crate::dfs::chunk_server_service_client::ChunkServerServiceClient::with_interceptor(channel, dfs_common::telemetry::propagation_interceptor(request_id))
                                .max_decoding_message_size(100 * 1024 * 1024);
                            let replicate_req = crate::dfs::ReplicateBlockRequest {
                                block_id: req.block_id,
                                data: req.data,
                                next_servers: remaining_servers,
                            };

                            if let Err(e) = client.replicate_block(replicate_req).await {
                                tracing::error!("Failed to replicate to {}: {}", next_server, e);
                            }
                        }
                        Err(e) => {
                            tracing::error!(
                                "Failed to connect to {} for replication: {}",
                                next_server,
                                e
                            );
                        }
                    },
                    Err(e) => tracing::error!("Invalid URL {}: {}", next_addr, e),
                }
            }

            Ok(Response::new(crate::dfs::ReplicateBlockResponse {
                success: true,
                error_message: "".to_string(),
            }))
        }
        .instrument(span)
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_checksum_verification() {
        let dir = tempdir().unwrap();
        let server = MyChunkServer::new(dir.path().to_path_buf(), vec![]);
        let block_id = "test_block";
        let data = b"Hello, world! This is a test block for checksum verification.";

        // Test write and verify
        server.write_block_local(block_id, data).unwrap();
        server.verify_block(block_id, data).unwrap();

        // Test corruption
        let path = dir.path().join(block_id);
        let mut file = fs::OpenOptions::new().write(true).open(&path).unwrap();
        file.write_all(b"Corrupted").unwrap(); // Overwrite beginning

        // Read corrupted data
        let mut file = fs::File::open(&path).unwrap();
        let mut corrupted_data = Vec::new();
        file.read_to_end(&mut corrupted_data).unwrap();

        // Verify should fail
        assert!(server.verify_block(block_id, &corrupted_data).is_err());
    }
}
