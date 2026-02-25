use crate::dfs::chunk_server_service_server::ChunkServerService;
use crate::dfs::{ReadBlockRequest, ReadBlockResponse, WriteBlockRequest, WriteBlockResponse};
use std::fs;
use std::io::{Read, Write};
use std::path::PathBuf;
use tonic::{Request, Response, Status};
use tracing::Instrument;

use dfs_common::sharding::ShardMap;
use lru::LruCache;
use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex};

/// Cached block data with metadata
#[derive(Clone, Debug)]
struct CachedBlock {
    data: Vec<u8>,
    size: usize,
}

#[derive(Debug, Clone)]
pub struct MyChunkServer {
    storage_dir: PathBuf,
    config_server_addrs: Vec<String>,
    pub shard_map: Arc<Mutex<ShardMap>>,
    // LRU cache for frequently accessed blocks
    // Cache size is in number of blocks (not bytes)
    block_cache: Arc<Mutex<LruCache<String, CachedBlock>>>,
    ca_cert_path: Option<String>,
    domain_name: Option<String>,
}

impl MyChunkServer {
    pub fn new(
        storage_dir: PathBuf,
        config_server_addrs: Vec<String>,
        ca_cert_path: Option<String>,
        domain_name: Option<String>,
    ) -> Self {
        fs::create_dir_all(&storage_dir).expect("Failed to create storage directory");

        // Load shard map from config file if available
        let shard_config_path = std::env::var("SHARD_CONFIG").ok();
        let shard_map = if let Some(path) = shard_config_path {
            tracing::info!("Loading shard config from {}", path);
            Arc::new(Mutex::new(
                dfs_common::sharding::load_shard_map_from_config(Some(&path), 100),
            ))
        } else {
            Arc::new(Mutex::new(ShardMap::new(100)))
        };

        // Initialize block cache with capacity for 100 blocks
        // This can be made configurable via environment variable
        let cache_capacity = std::env::var("BLOCK_CACHE_SIZE")
            .ok()
            .and_then(|s| s.parse::<usize>().ok())
            .unwrap_or(100);

        let block_cache = Arc::new(Mutex::new(LruCache::new(
            NonZeroUsize::new(cache_capacity).unwrap(),
        )));

        tracing::info!(
            "Initialized block cache with capacity: {} blocks",
            cache_capacity
        );

        MyChunkServer {
            storage_dir,
            config_server_addrs,
            shard_map,
            block_cache,
            ca_cert_path,
            domain_name,
        }
    }

    async fn connect_endpoint(&self, url: &str) -> anyhow::Result<tonic::transport::Channel> {
        let mut addr = url.to_string();
        if self.ca_cert_path.is_some() && addr.starts_with("http://") {
            addr = addr.replace("http://", "https://");
        }
        let resolved_addr = if addr.contains("://") {
            addr.clone()
        } else {
            format!("http://{}", addr)
        };

        let mut endpoint = tonic::transport::Endpoint::from_shared(resolved_addr.clone())?;

        if let Some(ca_path) = &self.ca_cert_path {
            let domain = self.domain_name.clone().unwrap_or_else(|| {
                addr.split("://")
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

        endpoint
            .connect()
            .await
            .map_err(|e| anyhow::anyhow!("Failed to connect to {}: {}", resolved_addr, e))
    }

    pub fn get_storage_dir(&self) -> PathBuf {
        self.storage_dir.clone()
    }

    pub async fn refresh_shard_map(&self) -> Result<(), String> {
        for config_addr in &self.config_server_addrs {
            use crate::dfs::config_service_client::ConfigServiceClient;
            match self.connect_endpoint(config_addr).await {
                Ok(channel) => {
                    let mut client = ConfigServiceClient::new(channel);
                    let request = tonic::Request::new(crate::dfs::FetchShardMapRequest {});
                    match client.fetch_shard_map(request).await {
                        Ok(response) => {
                            let resp = response.into_inner();
                            let num_shards = resp.shards.len();
                            let mut shard_map = self.shard_map.lock().unwrap();
                            // Update shard map from response
                            for (shard_id, peers) in resp.shards {
                                shard_map.add_shard(shard_id, peers.peers);
                            }
                            let num_masters = shard_map.get_all_masters().len();
                            tracing::info!(
                                "Refreshed shard map: {} shards, {} total masters found",
                                num_shards,
                                num_masters
                            );
                            return Ok(());
                        }
                        Err(e) => {
                            tracing::warn!("Failed to fetch shard map from {}: {}", config_addr, e)
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!("Failed to connect to config server {}: {}", config_addr, e)
                }
            }
        }
        Err("Failed to refresh shard map from any config server".to_string())
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
        let masters = {
            let shard_map = self.shard_map.lock().unwrap();
            shard_map.get_all_masters()
        };

        for master_addr in masters {
            match self.connect_endpoint(&master_addr).await {
                Ok(channel) => {
                    let mut client =
                        crate::dfs::master_service_client::MasterServiceClient::new(channel);
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

            match self.connect_endpoint(&location).await {
                Ok(channel) => {
                    let mut client =
                        crate::dfs::chunk_server_service_client::ChunkServerServiceClient::new(
                            channel,
                        );
                    let request = tonic::Request::new(crate::dfs::ReadBlockRequest {
                        block_id: block_id.to_string(),
                        offset: 0,
                        length: 0, // Read entire block
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
        let mut client = match self.connect_endpoint(target_addr).await {
            Ok(channel) => {
                crate::dfs::chunk_server_service_client::ChunkServerServiceClient::new(channel)
            }
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

    pub async fn run_background_scrubber(server: MyChunkServer, interval: std::time::Duration) {
        let storage_dir = server.storage_dir.clone();

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
                match self.connect_endpoint(next_server).await {
                    Ok(channel) => {
                        let mut client = crate::dfs::chunk_server_service_client::ChunkServerServiceClient::with_interceptor(channel, dfs_common::telemetry::propagation_interceptor(request_id.clone()))
                            .max_decoding_message_size(100 * 1024 * 1024);
                        let replicate_req = crate::dfs::ReplicateBlockRequest {
                            block_id: req.block_id.clone(),
                            data: req.data.clone(),
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
                    // Get total block size
                    let metadata = file.metadata().map_err(|e| {
                        Status::internal(format!("Failed to get file metadata: {}", e))
                    })?;
                    let total_size = metadata.len();

                    // Determine read parameters
                    let offset = req.offset;
                    let length = if req.length == 0 {
                        // If length is 0, read from offset to end of block
                        total_size.saturating_sub(offset)
                    } else {
                        req.length
                    };

                    // Validate offset
                    if offset >= total_size {
                        return Err(Status::out_of_range(format!(
                            "Offset {} exceeds block size {}",
                            offset, total_size
                        )));
                    }

                    // Calculate actual bytes to read (don't read past end of file)
                    let bytes_to_read = std::cmp::min(length, total_size - offset);

                    // Check cache for full block reads (offset=0, length=entire block)
                    let is_full_block_read = offset == 0 && bytes_to_read == total_size;

                    if is_full_block_read {
                        // Try to get from cache
                        if let Ok(mut cache) = self.block_cache.lock() {
                            if let Some(cached) = cache.get(&req.block_id) {
                                tracing::debug!("Cache hit for block {}", req.block_id);
                                return Ok(Response::new(ReadBlockResponse {
                                    data: cached.data.clone(),
                                    bytes_read: cached.size as u64,
                                    total_size,
                                }));
                            }
                            tracing::debug!("Cache miss for block {}", req.block_id);
                        }
                    }

                    // Seek to offset and read requested data
                    use std::io::{Read, Seek, SeekFrom};
                    file.seek(SeekFrom::Start(offset))
                        .map_err(|e| Status::internal(format!("Failed to seek: {}", e)))?;

                    let mut data = vec![0u8; bytes_to_read as usize];
                    file.read_exact(&mut data)
                        .map_err(|e| Status::internal(format!("Failed to read data: {}", e)))?;

                    // For partial reads, we verify only if reading the entire block
                    // For now, skip checksum verification for partial reads to improve performance
                    // TODO: Implement chunked checksum verification for partial reads
                    if offset == 0 && bytes_to_read == total_size {
                        // Verify checksums only for full block reads
                        if let Err(e) = self.verify_block(&req.block_id, &data) {
                            tracing::error!(
                                "CRITICAL: Data corruption detected for block {}: {}",
                                req.block_id,
                                e
                            );

                            // Attempt automatic recovery
                            tracing::warn!(
                                "Attempting automatic recovery for block {}",
                                req.block_id
                            );
                            match self.recover_block(&req.block_id).await {
                                Ok(_) => {
                                    tracing::info!(
                                        "Block {} successfully recovered, retrying read",
                                        req.block_id
                                    );
                                    // Re-read the recovered block
                                    let mut file = fs::File::open(&path)
                                        .map_err(|e| Status::internal(e.to_string()))?;
                                    let mut recovered_data = vec![0u8; bytes_to_read as usize];
                                    file.seek(SeekFrom::Start(offset))
                                        .map_err(|e| Status::internal(e.to_string()))?;
                                    file.read_exact(&mut recovered_data)
                                        .map_err(|e| Status::internal(e.to_string()))?;

                                    // Verify recovered data
                                    if let Err(e) =
                                        self.verify_block(&req.block_id, &recovered_data)
                                    {
                                        return Err(Status::data_loss(format!(
                                            "Recovered block is still corrupted: {}",
                                            e
                                        )));
                                    }

                                    return Ok(Response::new(ReadBlockResponse {
                                        data: recovered_data,
                                        bytes_read: bytes_to_read,
                                        total_size,
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
                    }

                    tracing::debug!(
                        "Read block {} (offset={}, length={}, bytes_read={})",
                        req.block_id,
                        offset,
                        length,
                        bytes_to_read
                    );

                    // Cache full block reads
                    if is_full_block_read {
                        if let Ok(mut cache) = self.block_cache.lock() {
                            cache.put(
                                req.block_id.clone(),
                                CachedBlock {
                                    data: data.clone(),
                                    size: data.len(),
                                },
                            );
                            tracing::debug!("Cached block {}", req.block_id);
                        }
                    }

                    Ok(Response::new(ReadBlockResponse {
                        data,
                        bytes_read: bytes_to_read,
                        total_size,
                    }))
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

                match self.connect_endpoint(next_server).await {
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
        let server = MyChunkServer::new(dir.path().to_path_buf(), vec![], None, None);
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
