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

pub const MAX_GRPC_MESSAGE_SIZE: usize = 100 * 1024 * 1024;
pub const CHECKSUM_CHUNK_SIZE: usize = 512;

/// Cached block data with metadata
#[derive(Clone, Debug)]
struct CachedBlock {
    data: Vec<u8>,
    size: usize,
}

#[derive(Debug, Clone)]
pub struct MyChunkServer {
    storage_dir: PathBuf,
    cold_storage_dir: Option<PathBuf>, // None = tiering disabled
    config_server_addrs: Vec<String>,
    pub shard_map: Arc<Mutex<ShardMap>>,
    // LRU cache for frequently accessed blocks
    // Cache size is in number of blocks (not bytes)
    block_cache: Arc<Mutex<LruCache<String, CachedBlock>>>,
    ca_cert_path: Option<String>,
    domain_name: Option<String>,
    /// Corrupted block IDs detected by the scrubber, waiting to be reported to Master.
    pub pending_bad_blocks: Arc<Mutex<Vec<String>>>,
}

impl MyChunkServer {
    pub fn new(
        storage_dir: PathBuf,
        cold_storage_dir: Option<PathBuf>,
        config_server_addrs: Vec<String>,
        ca_cert_path: Option<String>,
        domain_name: Option<String>,
    ) -> Self {
        fs::create_dir_all(&storage_dir).expect("Failed to create storage directory");
        if let Some(ref cold) = cold_storage_dir {
            fs::create_dir_all(cold).expect("Failed to create cold storage directory");
        }

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
            cold_storage_dir,
            config_server_addrs,
            shard_map,
            block_cache,
            ca_cert_path,
            domain_name,
            pending_bad_blocks: Arc::new(Mutex::new(Vec::new())),
        }
    }

    async fn connect_endpoint(&self, url: &str) -> anyhow::Result<tonic::transport::Channel> {
        dfs_common::security::connect_endpoint(
            url,
            self.ca_cert_path.as_deref(),
            self.domain_name.as_deref(),
        )
        .await
    }

    pub fn get_storage_dir(&self) -> PathBuf {
        self.storage_dir.clone()
    }

    /// Returns the path to a block file, checking hot dir first then cold dir.
    fn block_path(&self, block_id: &str) -> PathBuf {
        let hot = self.storage_dir.join(block_id);
        if hot.exists() {
            return hot;
        }
        if let Some(ref cold) = self.cold_storage_dir {
            let cold_path = cold.join(block_id);
            if cold_path.exists() {
                return cold_path;
            }
        }
        hot // fallback — will produce NotFound naturally on read
    }

    /// Atomically moves a block (and its .meta file) from hot to cold storage.
    pub async fn move_block_to_cold(&self, block_id: &str) -> std::io::Result<()> {
        let cold_dir = self
            .cold_storage_dir
            .as_ref()
            .ok_or_else(|| std::io::Error::other("cold_storage_dir not configured"))?;
        let src = self.storage_dir.join(block_id);
        let src_meta = self.storage_dir.join(format!("{}.meta", block_id));
        let dst = cold_dir.join(block_id);
        let dst_meta = cold_dir.join(format!("{}.meta", block_id));
        tokio::task::spawn_blocking(move || {
            std::fs::rename(&src, &dst)?;
            if src_meta.exists() {
                std::fs::rename(&src_meta, &dst_meta)?;
            }
            Ok::<(), std::io::Error>(())
        })
        .await
        .map_err(|e| std::io::Error::other(e.to_string()))?
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
        for chunk in data.chunks(CHECKSUM_CHUNK_SIZE) {
            let mut hasher = crc32fast::Hasher::new();
            hasher.update(chunk);
            checksums.push(hasher.finalize());
        }
        checksums
    }

    async fn write_block_async(&self, block_id: &str, data: &[u8]) -> Result<(), std::io::Error> {
        let path = self.storage_dir.join(block_id);
        let meta_path = self.storage_dir.join(format!("{}.meta", block_id));
        let checksums = Self::calculate_checksums(data);
        let meta_bytes: Vec<u8> = checksums.iter().flat_map(|c| c.to_be_bytes()).collect();
        let data_owned = data.to_vec();
        tokio::task::spawn_blocking(move || {
            let mut file = std::fs::File::create(&path)?;
            file.write_all(&data_owned)?;
            file.sync_all()?;
            let mut meta_file = std::fs::File::create(&meta_path)?;
            meta_file.write_all(&meta_bytes)?;
            meta_file.sync_all()?;
            Ok::<(), std::io::Error>(())
        })
        .await
        .map_err(|e| std::io::Error::other(e.to_string()))?
    }

    async fn read_block_async(
        &self,
        block_id: &str,
        offset: u64,
        length: u64,
    ) -> Result<Vec<u8>, std::io::Error> {
        use std::io::{Seek, SeekFrom};
        let path = self.block_path(block_id);
        let total_size = std::fs::metadata(&path)?.len();
        if offset >= total_size {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("Offset {} exceeds file size {}", offset, total_size),
            ));
        }
        let bytes_to_read = std::cmp::min(length, total_size - offset) as usize;
        tokio::task::spawn_blocking(move || {
            let mut file = std::fs::File::open(&path)?;
            file.seek(SeekFrom::Start(offset))?;
            let mut buf = vec![0u8; bytes_to_read];
            file.read_exact(&mut buf)?;
            Ok::<Vec<u8>, std::io::Error>(buf)
        })
        .await
        .map_err(|e| std::io::Error::other(e.to_string()))?
    }

    fn verify_block(&self, block_id: &str, data: &[u8]) -> Result<(), String> {
        // Check hot storage first, then cold storage for the .meta file
        let hot_meta = self.storage_dir.join(format!("{}.meta", block_id));
        let meta_path = if hot_meta.exists() {
            hot_meta
        } else if let Some(ref cold) = self.cold_storage_dir {
            let cold_meta = cold.join(format!("{}.meta", block_id));
            if cold_meta.exists() {
                cold_meta
            } else {
                hot_meta // fallback — will produce "missing" error below
            }
        } else {
            hot_meta
        };
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

    /// Verify checksums for a partial read by checking only the affected chunks.
    /// `offset` and `length` define the byte range that was read from the block.
    fn verify_partial_read(&self, block_id: &str, offset: u64, length: u64) -> Result<(), String> {
        let meta_path = self.storage_dir.join(format!("{}.meta", block_id));
        if !meta_path.exists() {
            return Err("Checksum file missing".to_string());
        }

        let mut meta_file = fs::File::open(&meta_path).map_err(|e| e.to_string())?;
        let mut meta_data = Vec::new();
        meta_file
            .read_to_end(&mut meta_data)
            .map_err(|e| e.to_string())?;

        let all_checksums: Vec<u32> = meta_data
            .chunks_exact(4)
            .map(|chunk| {
                let bytes: [u8; 4] = chunk
                    .try_into()
                    .map_err(|_| "Invalid checksum size".to_string())?;
                Ok(u32::from_be_bytes(bytes))
            })
            .collect::<Result<Vec<u32>, String>>()?;

        let start_chunk = (offset as usize) / CHECKSUM_CHUNK_SIZE;
        let end_chunk = ((offset + length).saturating_sub(1) as usize) / CHECKSUM_CHUNK_SIZE;

        // Read the affected chunk range from the block file for verification
        let block_path = self.storage_dir.join(block_id);
        let mut file = fs::File::open(&block_path).map_err(|e| e.to_string())?;
        let chunk_start_byte = start_chunk * CHECKSUM_CHUNK_SIZE;

        use std::io::{Seek, SeekFrom};
        file.seek(SeekFrom::Start(chunk_start_byte as u64))
            .map_err(|e| e.to_string())?;

        let file_size = file.metadata().map_err(|e| e.to_string())?.len() as usize;

        for i in start_chunk..=end_chunk {
            if i >= all_checksums.len() {
                break;
            }
            let chunk_offset = i * CHECKSUM_CHUNK_SIZE;
            let chunk_len = std::cmp::min(CHECKSUM_CHUNK_SIZE, file_size - chunk_offset);
            let mut buf = vec![0u8; chunk_len];
            file.read_exact(&mut buf).map_err(|e| e.to_string())?;

            let mut hasher = crc32fast::Hasher::new();
            hasher.update(&buf);
            let actual = hasher.finalize();

            if actual != all_checksums[i] {
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
                                if let Err(e) = self.write_block_async(block_id, &data).await {
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
        let path = self.block_path(block_id);
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
            expected_checksum_crc32c: 0,
        });

        match client.replicate_block(request).await {
            Ok(_) => Ok(()),
            Err(e) => Err(format!("Replication failed: {}", e)),
        }
    }

    /// Reconstruct an EC shard using Reed-Solomon from available peer shards.
    /// Called when Master issues a RECONSTRUCT_EC_SHARD command.
    pub async fn reconstruct_ec_shard(
        &self,
        block_id: String,
        shard_index: usize,
        data_shards: usize,
        parity_shards: usize,
        sources: Vec<String>, // one address per shard slot; empty = unavailable
    ) -> anyhow::Result<()> {
        let total = data_shards + parity_shards;
        if sources.len() != total {
            return Err(anyhow::anyhow!(
                "ec_shard_sources length {} != total shards {}",
                sources.len(),
                total
            ));
        }

        // 1. Fetch available shards concurrently
        type ShardFuture = std::pin::Pin<
            Box<dyn std::future::Future<Output = (usize, anyhow::Result<Vec<u8>>)> + Send>,
        >;
        let mut fetch_futures: Vec<ShardFuture> = Vec::new();

        for (i, src_addr) in sources.iter().enumerate() {
            // Skip the shard we're reconstructing — RS::reconstruct needs opt_shards[shard_index] = None
            // to know which shard to fill in. Also skip unavailable sources (empty addr).
            if src_addr.is_empty() || i == shard_index {
                continue;
            }
            let block_id_c = block_id.clone();
            let addr = format!("http://{}", src_addr);
            let ca_cert_path = self.ca_cert_path.clone();
            let domain_name = self.domain_name.clone();

            fetch_futures.push(Box::pin(async move {
                // Build endpoint (reuse TLS logic if needed)
                let resolved = if ca_cert_path.is_some() {
                    addr.replace("http://", "https://")
                } else {
                    addr.clone()
                };
                let mut endpoint = match tonic::transport::Endpoint::from_shared(resolved.clone()) {
                    Ok(e) => e,
                    Err(e) => return (i, Err(anyhow::anyhow!("Bad endpoint {}: {}", resolved, e))),
                };
                if let Some(ref ca_path) = ca_cert_path {
                    let domain = domain_name.clone().unwrap_or_else(|| {
                        resolved
                            .split("://")
                            .last()
                            .unwrap_or("")
                            .split(':')
                            .next()
                            .unwrap_or("localhost")
                            .to_string()
                    });
                    if let Ok(tls_config) =
                        dfs_common::security::get_client_tls_config(ca_path, &domain)
                    {
                        endpoint = match endpoint.tls_config(tls_config) {
                            Ok(e) => e,
                            Err(e) => return (i, Err(anyhow::anyhow!("TLS config error: {}", e))),
                        };
                    }
                }
                let channel = match endpoint.connect().await {
                    Ok(c) => c,
                    Err(e) => {
                        return (
                            i,
                            Err(anyhow::anyhow!("Connect failed to {}: {}", resolved, e)),
                        )
                    }
                };
                let mut client =
                    crate::dfs::chunk_server_service_client::ChunkServerServiceClient::new(channel)
                        .max_decoding_message_size(MAX_GRPC_MESSAGE_SIZE);
                let req = tonic::Request::new(crate::dfs::ReadBlockRequest {
                    block_id: block_id_c.clone(),
                    offset: 0,
                    length: 0, // read entire block
                });
                match client.read_block(req).await {
                    Ok(resp) => (i, Ok(resp.into_inner().data)),
                    Err(e) => (
                        i,
                        Err(anyhow::anyhow!("ReadBlock failed from {}: {}", resolved, e)),
                    ),
                }
            }));
        }

        let results = futures_util::future::join_all(fetch_futures).await;

        // 2. Populate opt_shards
        let mut opt_shards: Vec<Option<Vec<u8>>> = vec![None; total];
        for (i, result) in results {
            match result {
                Ok(data) => opt_shards[i] = Some(data),
                Err(e) => tracing::warn!("EC fetch shard {}: {}", i, e),
            }
        }

        // Verify we have enough shards to reconstruct
        let available = opt_shards.iter().filter(|s| s.is_some()).count();
        // RS requires at least data_shards shards present to reconstruct any missing one.
        // Note: opt_shards[shard_index] is intentionally None (being reconstructed),
        // so available counts only the other shards.
        if available < data_shards {
            return Err(anyhow::anyhow!(
                "Only {} shards available, need at least {} for reconstruction",
                available,
                data_shards
            ));
        }

        // 3. RS reconstruct (fills in all missing shards)
        let r = reed_solomon_erasure::galois_8::ReedSolomon::new(data_shards, parity_shards)
            .map_err(|e| anyhow::anyhow!("RS init error: {:?}", e))?;
        r.reconstruct(&mut opt_shards)
            .map_err(|e| anyhow::anyhow!("RS reconstruct error: {:?}", e))?;

        // 4. Write the target shard to local storage
        let shard_data = opt_shards[shard_index]
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Shard {} still None after reconstruct", shard_index))?;
        self.write_block_async(&block_id, shard_data)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to write reconstructed shard: {}", e))?;

        tracing::info!(
            "EC reconstruct: wrote shard {} of block {} ({} bytes)",
            shard_index,
            block_id,
            shard_data.len()
        );
        Ok(())
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
                    // Queue for reporting to Master via next heartbeat
                    {
                        let mut pending = server.pending_bad_blocks.lock().expect("Mutex poisoned");
                        pending.extend(corrupted_blocks.iter().cloned());
                    }
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

            // Verify checksum if provided
            if req.expected_checksum_crc32c != 0 {
                let mut hasher = crc32fast::Hasher::new();
                hasher.update(&req.data);
                let actual_checksum = hasher.finalize();
                if actual_checksum != req.expected_checksum_crc32c {
                    tracing::error!(
                        "Checksum mismatch for block {}: expected {}, actual {}",
                        req.block_id,
                        req.expected_checksum_crc32c,
                        actual_checksum
                    );
                    return Ok(Response::new(WriteBlockResponse {
                        success: false,
                        error_message: format!(
                            "Checksum mismatch: expected {}, actual {}",
                            req.expected_checksum_crc32c, actual_checksum
                        ),
                        ..Default::default()
                    }));
                }
            }

            // Write block locally with checksums
            if let Err(e) = self.write_block_async(&req.block_id, &req.data).await {
                return Ok(Response::new(WriteBlockResponse {
                    success: false,
                    error_message: e.to_string(),
                    ..Default::default()
                }));
            }

            // If there are next servers in the pipeline, replicate to them
            let mut replicas_written = 1i32; // This server's local write

            if !req.next_servers.is_empty() {
                let next_server = &req.next_servers[0];
                let remaining_servers = req.next_servers[1..].to_vec();

                // Forward to next server in pipeline
                match self.connect_endpoint(next_server).await {
                    Ok(channel) => {
                        let mut client = crate::dfs::chunk_server_service_client::ChunkServerServiceClient::with_interceptor(channel, dfs_common::telemetry::propagation_interceptor(request_id.clone()))
                            .max_decoding_message_size(MAX_GRPC_MESSAGE_SIZE);
                        let replicate_req = crate::dfs::ReplicateBlockRequest {
                            block_id: req.block_id.clone(),
                            data: req.data.clone(),
                            next_servers: remaining_servers,
                            expected_checksum_crc32c: req.expected_checksum_crc32c,
                        };

                        match client.replicate_block(replicate_req).await {
                            Ok(resp) => {
                                let inner = resp.into_inner();
                                if inner.success {
                                    replicas_written += inner.replicas_written;
                                } else {
                                    tracing::error!("Downstream replication failed at {}: {}", next_server, inner.error_message);
                                }
                            }
                            Err(e) => {
                                tracing::error!("Failed to replicate to {}: {}", next_server, e);
                            }
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
                replicas_written,
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
            let path = self.block_path(&req.block_id);

            // Get total file size for validation and cache check
            let total_size = match std::fs::metadata(&path) {
                Ok(m) => m.len(),
                Err(_) => return Err(Status::not_found("Block not found")),
            };

            // Determine read parameters
            let offset = req.offset;
            let length = if req.length == 0 {
                total_size.saturating_sub(offset)
            } else {
                req.length
            };

            if offset >= total_size {
                return Err(Status::out_of_range(format!(
                    "Offset {} exceeds block size {}",
                    offset, total_size
                )));
            }

            let bytes_to_read = std::cmp::min(length, total_size - offset);
            let is_full_block_read = offset == 0 && bytes_to_read == total_size;

            // Fast path: LRU cache for full-block reads
            if is_full_block_read {
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

            // io_uring read (no seek syscall — read_at uses offset directly)
            let data = match self
                .read_block_async(&req.block_id, offset, bytes_to_read)
                .await
            {
                Ok(d) => d,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    return Err(Status::not_found("Block not found"));
                }
                Err(e) => return Err(Status::internal(format!("Failed to read block: {}", e))),
            };

            // Partial read: verify only the affected checksum chunks
            if !is_full_block_read {
                if let Err(e) = self.verify_partial_read(
                    &req.block_id,
                    offset,
                    bytes_to_read,
                ) {
                    tracing::warn!(
                        "Partial read checksum verification failed for block {} (offset={}, len={}): {}",
                        req.block_id, offset, bytes_to_read, e
                    );
                    // Don't fail the read for partial checksum errors,
                    // but trigger recovery in background
                    let server = self.clone();
                    let block_id = req.block_id.clone();
                    tokio::spawn(async move {
                        let _ = server.recover_block(&block_id).await;
                    });
                }
            }

            // Checksum verification for full-block reads (with auto-recovery)
            if is_full_block_read {
                if let Err(e) = self.verify_block(&req.block_id, &data) {
                    tracing::error!(
                        "CRITICAL: Data corruption detected for block {}: {}",
                        req.block_id,
                        e
                    );
                    tracing::warn!("Attempting automatic recovery for block {}", req.block_id);
                    match self.recover_block(&req.block_id).await {
                        Ok(_) => {
                            tracing::info!("Block {} recovered, retrying read", req.block_id);
                            let recovered = self
                                .read_block_async(&req.block_id, offset, bytes_to_read)
                                .await
                                .map_err(|e| Status::internal(e.to_string()))?;
                            if let Err(e) = self.verify_block(&req.block_id, &recovered) {
                                return Err(Status::data_loss(format!(
                                    "Recovered block is still corrupted: {}",
                                    e
                                )));
                            }
                            return Ok(Response::new(ReadBlockResponse {
                                data: recovered,
                                bytes_read: bytes_to_read,
                                total_size,
                            }));
                        }
                        Err(recovery_err) => {
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

            // Cache full-block reads for future requests
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

            // Verify checksum if provided (Phase 3: ChunkServer In-flight Verification)
            if req.expected_checksum_crc32c != 0 {
                let mut hasher = crc32fast::Hasher::new();
                hasher.update(&req.data);
                let actual_checksum = hasher.finalize();
                if actual_checksum != req.expected_checksum_crc32c {
                    tracing::error!(
                        "Checksum mismatch during replication for block {}: expected {}, actual {}",
                        req.block_id,
                        req.expected_checksum_crc32c,
                        actual_checksum
                    );
                    return Ok(Response::new(crate::dfs::ReplicateBlockResponse {
                        success: false,
                        error_message: format!(
                            "Replication checksum mismatch: expected {}, actual {}",
                            req.expected_checksum_crc32c, actual_checksum
                        ),
                        ..Default::default()
                    }));
                }
            }

            // Write block locally with checksums
            if let Err(e) = self.write_block_async(&req.block_id, &req.data).await {
                return Ok(Response::new(crate::dfs::ReplicateBlockResponse {
                    success: false,
                    error_message: e.to_string(),
                    ..Default::default()
                }));
            }

            let mut replicas_written = 1i32; // This server's local write

            if !req.next_servers.is_empty() {
                let next_server = &req.next_servers[0];
                let remaining_servers = req.next_servers[1..].to_vec();

                match self.connect_endpoint(next_server).await {
                    Ok(channel) => {
                        let mut client = crate::dfs::chunk_server_service_client::ChunkServerServiceClient::with_interceptor(channel, dfs_common::telemetry::propagation_interceptor(request_id))
                            .max_decoding_message_size(MAX_GRPC_MESSAGE_SIZE);
                        let replicate_req = crate::dfs::ReplicateBlockRequest {
                            block_id: req.block_id,
                            data: req.data,
                            next_servers: remaining_servers,
                            expected_checksum_crc32c: req.expected_checksum_crc32c,
                        };

                        match client.replicate_block(replicate_req).await {
                            Ok(resp) => {
                                let inner = resp.into_inner();
                                if inner.success {
                                    replicas_written += inner.replicas_written;
                                } else {
                                    tracing::error!("Downstream replication failed at {}: {}", next_server, inner.error_message);
                                }
                            }
                            Err(e) => {
                                tracing::error!("Failed to replicate to {}: {}", next_server, e);
                            }
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
                replicas_written,
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

    fn make_test_server(dir: &std::path::Path) -> MyChunkServer {
        MyChunkServer::new(dir.to_path_buf(), None, vec![], None, None)
    }

    #[test]
    fn test_ec_reconstruct_shard_logic() {
        // Verify RS reconstruction produces correct shard
        let data = vec![7u8; 4096];
        let shards = dfs_common::erasure::encode(&data, 4, 2).unwrap();

        // Shard 4 (first parity) is "missing"
        let mut opt: Vec<Option<Vec<u8>>> = shards.iter().map(|s| Some(s.clone())).collect();
        opt[4] = None;

        // Reconstruct using the erasure module
        let r = reed_solomon_erasure::galois_8::ReedSolomon::new(4, 2).unwrap();
        r.reconstruct(&mut opt).unwrap();

        let reconstructed = opt[4].as_ref().unwrap();
        assert_eq!(reconstructed, &shards[4]);
    }

    #[tokio::test]
    async fn test_checksum_verification() {
        let dir = tempdir().unwrap();
        let server = MyChunkServer::new(dir.path().to_path_buf(), None, vec![], None, None);
        let block_id = "test_block";
        let data = b"Hello, world! This is a test block for checksum verification.";

        // Test write and verify
        server.write_block_async(block_id, data).await.unwrap();
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

    fn make_test_data(size: usize) -> Vec<u8> {
        (0..size).map(|i| (i % 256) as u8).collect()
    }

    #[tokio::test]
    async fn test_write_block_async_creates_file_with_correct_content() {
        let dir = tempfile::TempDir::new().unwrap();
        let server = make_test_server(dir.path());
        let data = b"hello io_uring world";
        server
            .write_block_async("test-block-async", data)
            .await
            .unwrap();
        let written = std::fs::read(dir.path().join("test-block-async")).unwrap();
        assert_eq!(written.as_slice(), data.as_ref());
    }

    #[tokio::test]
    async fn test_write_block_async_creates_meta_file() {
        let dir = tempfile::TempDir::new().unwrap();
        let server = make_test_server(dir.path());
        let data = b"checksum test data for io_uring";
        server
            .write_block_async("meta-block-async", data)
            .await
            .unwrap();
        assert!(dir.path().join("meta-block-async.meta").exists());
        let meta = std::fs::read(dir.path().join("meta-block-async.meta")).unwrap();
        assert!(!meta.is_empty());
    }

    #[tokio::test]
    async fn test_read_block_async_returns_correct_data() {
        let dir = tempfile::TempDir::new().unwrap();
        let server = make_test_server(dir.path());
        let data = make_test_data(4096);
        std::fs::write(dir.path().join("read-test"), &data).unwrap();
        let result = server
            .read_block_async("read-test", 0, data.len() as u64)
            .await
            .unwrap();
        assert_eq!(result, data);
    }

    #[tokio::test]
    async fn test_read_block_async_partial_read_at_offset() {
        let dir = tempfile::TempDir::new().unwrap();
        let server = make_test_server(dir.path());
        let data = make_test_data(65536);
        std::fs::write(dir.path().join("partial-test"), &data).unwrap();
        let result = server
            .read_block_async("partial-test", 32768, 4096)
            .await
            .unwrap();
        assert_eq!(result.len(), 4096);
        assert_eq!(result, data[32768..32768 + 4096].to_vec());
    }

    #[tokio::test]
    async fn test_move_block_to_cold() {
        let hot_dir = tempfile::TempDir::new().unwrap();
        let cold_dir = tempfile::TempDir::new().unwrap();
        let server = MyChunkServer::new(
            hot_dir.path().to_path_buf(),
            Some(cold_dir.path().to_path_buf()),
            vec![],
            None,
            None,
        );
        let data = b"cold block data";
        server.write_block_async("cold-block", data).await.unwrap();
        assert!(
            hot_dir.path().join("cold-block").exists(),
            "should be in hot dir after write"
        );
        server.move_block_to_cold("cold-block").await.unwrap();
        assert!(
            !hot_dir.path().join("cold-block").exists(),
            "should no longer be in hot dir"
        );
        assert!(
            cold_dir.path().join("cold-block").exists(),
            "should now be in cold dir"
        );
    }

    #[tokio::test]
    async fn test_read_block_finds_cold_block() {
        let hot_dir = tempfile::TempDir::new().unwrap();
        let cold_dir = tempfile::TempDir::new().unwrap();
        let server = MyChunkServer::new(
            hot_dir.path().to_path_buf(),
            Some(cold_dir.path().to_path_buf()),
            vec![],
            None,
            None,
        );
        let data: Vec<u8> = (0u8..16).collect();
        // Write directly to cold dir to simulate an already-migrated block
        std::fs::write(cold_dir.path().join("cold-test"), &data).unwrap();
        let result = server
            .read_block_async("cold-test", 0, data.len() as u64)
            .await
            .unwrap();
        assert_eq!(result, data);
    }
}
