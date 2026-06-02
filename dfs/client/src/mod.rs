pub mod dfs {
    include!(concat!(env!("OUT_DIR"), "/dfs.rs"));
}

use crate::dfs::chunk_server_service_client::ChunkServerServiceClient;
use crate::dfs::master_service_client::MasterServiceClient;
use crate::dfs::{
    AllocateBlockRequest, CompleteFileRequest, CreateFileRequest, DeleteFileRequest,
    GetFileInfoRequest, ListFilesRequest, ReadBlockRequest, RenameRequest, WriteBlockRequest,
};
use anyhow::{anyhow, bail};
use dfs_common::sharding::ShardMap;
use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Write};
use std::path::Path;
use std::sync::{Arc, RwLock};
use tokio::time::{sleep, Duration};
use tonic::transport::Channel;

const MAX_RETRIES: usize = 5;
const INITIAL_BACKOFF_MS: u64 = 500;

#[derive(Clone)]
pub struct Client {
    master_addrs: Vec<String>,
    shard_map: Arc<RwLock<ShardMap>>,
    host_aliases: Arc<RwLock<HashMap<String, String>>>,
    config_server_addrs: Vec<String>,
    max_retries: usize,
    initial_backoff_ms: u64,
    ca_cert_path: Option<String>,
    domain_name: Option<String>,
    /// Hedge delay in milliseconds. None = hedging disabled.
    hedge_delay_ms: Option<u64>,
}

impl Client {
    pub fn new(master_addrs: Vec<String>, config_server_addrs: Vec<String>) -> Self {
        // Initialize with default (empty) ShardMap.
        Self {
            master_addrs,
            shard_map: Arc::new(RwLock::new(ShardMap::new(100))),
            host_aliases: Arc::new(RwLock::new(HashMap::new())),
            config_server_addrs,
            max_retries: MAX_RETRIES,
            initial_backoff_ms: INITIAL_BACKOFF_MS,
            ca_cert_path: None,
            domain_name: None,
            hedge_delay_ms: None,
        }
    }

    pub fn with_tls_config(
        mut self,
        ca_cert_path: Option<String>,
        domain_name: Option<String>,
    ) -> Self {
        self.ca_cert_path = ca_cert_path;
        self.domain_name = domain_name;
        self
    }

    pub fn with_retry_config(mut self, max_retries: usize, initial_backoff_ms: u64) -> Self {
        self.max_retries = max_retries;
        self.initial_backoff_ms = initial_backoff_ms;
        self
    }

    /// Enable hedged reads: if the primary replica doesn't respond within
    /// `delay_ms` milliseconds, a second request is launched to the next replica.
    /// The first successful response wins.
    pub fn with_hedge_delay(mut self, delay_ms: u64) -> Self {
        self.hedge_delay_ms = Some(delay_ms);
        self
    }

    pub fn set_shard_map(&self, map: ShardMap) {
        let mut w = self.shard_map.write().unwrap();
        *w = map;
    }

    pub fn add_host_alias(&self, alias: impl Into<String>, real: impl Into<String>) {
        let mut w = self.host_aliases.write().unwrap();
        w.insert(alias.into(), real.into());
    }

    fn resolve_url(&self, url: &str) -> String {
        let map = self.host_aliases.read().unwrap();
        for (alias, real) in map.iter() {
            if url.contains(alias) {
                return url.replace(alias, real);
            }
        }
        url.to_string()
    }

    async fn connect_endpoint(&self, url: &str) -> anyhow::Result<Channel> {
        let mut addr = url.to_string();
        if self.ca_cert_path.is_some() && addr.starts_with("http://") {
            addr = addr.replace("http://", "https://");
        }
        let resolved_addr = self.resolve_url(&addr);
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
            .map_err(|e| anyhow!("Failed to connect to {}: {}", resolved_addr, e))
    }

    pub async fn list_files(&self, path: &str) -> anyhow::Result<Vec<String>> {
        let (response, _) = self
            .execute_rpc(Some(path), |mut client| {
                let path = path.to_string();
                async move {
                    let request = tonic::Request::new(ListFilesRequest { path });
                    client.list_files(request).await
                }
            })
            .await?;

        Ok(response.into_inner().files)
    }

    pub async fn list_all_files(&self) -> anyhow::Result<Vec<String>> {
        // First, refresh shard map from config server if configured
        if !self.config_server_addrs.is_empty() {
            let _ = self.refresh_shard_map().await;
        }

        let (shards, default_masters) = {
            let map = self.shard_map.read().unwrap();
            (map.get_all_shards(), self.master_addrs.clone())
        };

        let mut all_files = std::collections::HashSet::new();

        if shards.is_empty() {
            // No shards configured, use default masters as single shard
            let (response, _) = self
                .execute_rpc_internal(
                    &default_masters,
                    self.max_retries,
                    self.initial_backoff_ms,
                    |mut client| async move {
                        let request = tonic::Request::new(ListFilesRequest {
                            path: "/".to_string(),
                        });
                        client.list_files(request).await
                    },
                )
                .await?;
            for f in response.into_inner().files {
                all_files.insert(f);
            }
        } else {
            // Query each shard
            for shard_id in shards {
                let peers = {
                    let map = self.shard_map.read().unwrap();
                    map.get_shard_peers(&shard_id).unwrap_or_default()
                };

                if peers.is_empty() {
                    continue;
                }

                // We try to query the shard. If it fails, we log and continue (partial results better than crash?)
                // Ideally we should fail if any shard is unreachable to be consistent.
                // Let's fail if we can't get data from a shard.
                let result = self
                    .execute_rpc_internal(
                        &peers,
                        self.max_retries,
                        self.initial_backoff_ms,
                        |mut client| async move {
                            let request = tonic::Request::new(ListFilesRequest {
                                path: "/".to_string(),
                            });
                            client.list_files(request).await
                        },
                    )
                    .await;

                match result {
                    Ok((response, _)) => {
                        for f in response.into_inner().files {
                            all_files.insert(f);
                        }
                    }
                    Err(e) => {
                        tracing::warn!("Failed to list files from shard {}: {}", shard_id, e);
                        return Err(e);
                    }
                }
            }
        }

        Ok(all_files.into_iter().collect())
    }

    pub async fn create_file(&self, source: &Path, dest: &str) -> anyhow::Result<()> {
        // 1. Read local file
        let mut file = File::open(source)?;
        let mut buffer = Vec::new();
        file.read_to_end(&mut buffer)?;

        self.create_file_from_buffer(buffer, dest).await
    }

    pub async fn create_file_from_buffer(&self, buffer: Vec<u8>, dest: &str) -> anyhow::Result<()> {
        let buffer_len = buffer.len() as u64;

        // 1. Create file on Master
        let (create_resp, success_addr) = self
            .execute_rpc(Some(dest), |mut client| {
                let dest = dest.to_string();
                async move {
                    let create_req = tonic::Request::new(CreateFileRequest {
                        path: dest,
                        ec_data_shards: 0,
                        ec_parity_shards: 0,
                    });
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
            })
            .await?;
        let create_resp = create_resp.into_inner();

        if !create_resp.success {
            bail!("Failed to create file: {}", create_resp.error_message);
        }

        // 3. Allocate block
        // Use the master that handled create_file successfully to ensure read-your-writes consistency
        let alloc_masters = {
            let mut m = vec![success_addr];
            for addr in &self.master_addrs {
                if !m.contains(addr) {
                    m.push(addr.clone());
                }
            }
            m
        };

        let (alloc_resp, _) = self
            .execute_rpc_internal(
                &alloc_masters,
                self.max_retries,
                self.initial_backoff_ms,
                |mut client| {
                    let dest = dest.to_string();
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
            .await?;
        let alloc_resp = alloc_resp.into_inner();

        let block = alloc_resp
            .block
            .ok_or_else(|| anyhow!("No block allocated"))?;
        let chunk_servers = alloc_resp.chunk_server_addresses;

        if chunk_servers.is_empty() {
            bail!("No chunk servers available");
        }

        tracing::info!(
            block_id = %block.block_id,
            chunk_servers = ?chunk_servers,
            "Writing block to chunk servers"
        );

        // 4a. EC write path: encode into k+m shards and write in parallel
        let is_ec = alloc_resp.ec_data_shards > 0 && alloc_resp.ec_parity_shards > 0;
        if is_ec {
            let data_shards = alloc_resp.ec_data_shards as usize;
            let parity_shards = alloc_resp.ec_parity_shards as usize;
            let total_shards = data_shards + parity_shards;
            let original_size = buffer.len() as u64;

            if chunk_servers.len() != total_shards {
                bail!(
                    "Expected {} chunk servers for EC({},{}), got {}",
                    total_shards, data_shards, parity_shards, chunk_servers.len()
                );
            }

            let shards = dfs_common::erasure::encode(&buffer, data_shards, parity_shards)?;

            // Compute CRC32C checksum for each shard before writing
            let shard_checksums: Vec<u32> = shards.iter().map(|s| {
                let mut h = crc32fast::Hasher::new();
                h.update(s);
                h.finalize()
            }).collect();

            // Compute CRC32C of the original (pre-encoding) buffer for the block-level checksum
            let mut full_hasher = crc32fast::Hasher::new();
            full_hasher.update(&buffer);
            let full_checksum = full_hasher.finalize();

            // Write all shards in parallel (no pipeline, each goes to its own CS)
            let mut write_futs = Vec::new();
            for (shard_idx, (shard_data, cs_addr)) in
                shards.into_iter().zip(chunk_servers.iter()).enumerate()
            {
                let block_id = block.block_id.clone();
                let cs_url = format!("http://{}", cs_addr);
                let self_clone = self.clone();
                let shard_idx_i32 = shard_idx as i32;
                let checksum = shard_checksums[shard_idx];
                write_futs.push(async move {
                    let channel = self_clone.connect_endpoint(&cs_url).await?;
                    let mut cs_client = crate::dfs::chunk_server_service_client::ChunkServerServiceClient::with_interceptor(
                        channel,
                        dfs_common::telemetry::tracing_interceptor
                            as fn(tonic::Request<()>) -> Result<tonic::Request<()>, tonic::Status>,
                    )
                    .max_decoding_message_size(100 * 1024 * 1024);
                    let req = tonic::Request::new(WriteBlockRequest {
                        block_id: block_id.clone(),
                        data: shard_data,
                        next_servers: vec![],
                        expected_checksum_crc32c: checksum,
                        shard_index: shard_idx_i32,
                    });
                    let resp = cs_client.write_block(req).await?.into_inner();
                    if !resp.success {
                        bail!("Shard {} write failed: {}", shard_idx_i32, resp.error_message);
                    }
                    Ok::<(), anyhow::Error>(())
                });
            }
            futures_util::future::try_join_all(write_futs).await?;

            // Complete file with EC block info (actual_size = original unpadded size)
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis() as u64;

            let ec_block_checksum = crate::dfs::BlockChecksumInfo {
                block_id: block.block_id.clone(),
                checksum_crc32c: full_checksum,
                actual_size: original_size,
            };

            let (complete_resp, _) = self
                .execute_rpc(Some(dest), |mut client| {
                    let dest = dest.to_string();
                    let blocks = vec![ec_block_checksum.clone()];
                    async move {
                        let complete_req = tonic::Request::new(CompleteFileRequest {
                            path: dest,
                            size: original_size,
                            etag_md5: "".to_string(),
                            created_at_ms: now,
                            block_checksums: blocks,
                        });
                        client.complete_file(complete_req).await
                    }
                })
                .await?;

            if !complete_resp.into_inner().success {
                bail!("Failed to complete EC file");
            }

            return Ok(());
        }

        // 4b. Replication write path (non-EC)
        let chunk_server_addr = format!("http://{}", chunk_servers[0]);
        let channel = self.connect_endpoint(&chunk_server_addr).await?;
        let mut chunk_client =
            crate::dfs::chunk_server_service_client::ChunkServerServiceClient::with_interceptor(
                channel,
                dfs_common::telemetry::tracing_interceptor
                    as fn(tonic::Request<()>) -> Result<tonic::Request<()>, tonic::Status>,
            )
            .max_decoding_message_size(100 * 1024 * 1024);

        // Calculate checksums
        let mut hasher_crc = crc32fast::Hasher::new();
        hasher_crc.update(&buffer);
        let checksum_crc32c = hasher_crc.finalize();

        let etag_md5 = format!("{:x}", md5::compute(&buffer));

        let block_checksums = vec![crate::dfs::BlockChecksumInfo {
            block_id: block.block_id.clone(),
            checksum_crc32c,
            actual_size: buffer_len,
        }];

        let next_servers = chunk_servers[1..].to_vec();

        let write_req = tonic::Request::new(WriteBlockRequest {
            block_id: block.block_id,
            data: buffer,
            next_servers,
            expected_checksum_crc32c: checksum_crc32c,
            shard_index: -1,  // -1 = replicated (not EC)
        });

        let write_resp = chunk_client.write_block(write_req).await?.into_inner();
        if !write_resp.success {
            bail!("Failed to write block: {}", write_resp.error_message);
        }

        // 5. Complete file
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;

        let (complete_resp, _) = self
            .execute_rpc(Some(dest), |mut client| {
                let dest = dest.to_string();
                let size = buffer_len;
                let etag = etag_md5.clone();
                let blocks = block_checksums.clone();
                async move {
                    let complete_req = tonic::Request::new(CompleteFileRequest {
                        path: dest,
                        size,
                        etag_md5: etag,
                        created_at_ms: now,
                        block_checksums: blocks,
                    });
                    client.complete_file(complete_req).await
                }
            })
            .await?;

        if !complete_resp.into_inner().success {
            bail!("Failed to complete file");
        }

        Ok(())
    }

    pub async fn get_file_info(
        &self,
        path: &str,
    ) -> anyhow::Result<Option<crate::dfs::FileMetadata>> {
        let (info_resp, _) = self
            .execute_rpc(Some(path), |mut client| {
                let path = path.to_string();
                async move {
                    let info_req = tonic::Request::new(GetFileInfoRequest { path });
                    client.get_file_info(info_req).await
                }
            })
            .await?;
        Ok(info_resp.into_inner().metadata)
    }

    pub async fn get_file(&self, source: &str, dest: &Path) -> anyhow::Result<()> {
        // 1. Get file info from Master
        let (info_resp, _) = self
            .execute_rpc(Some(source), |mut client| {
                let source = source.to_string();
                async move {
                    let info_req = tonic::Request::new(GetFileInfoRequest { path: source });
                    client.get_file_info(info_req).await
                }
            })
            .await?;
        let info_resp = info_resp.into_inner();

        if !info_resp.found {
            bail!("File not found");
        }

        let metadata = info_resp.metadata.ok_or_else(|| anyhow!("No metadata"))?;
        let mut file = File::create(dest)?;

        // 2. Read blocks from ChunkServers
        for block in metadata.blocks {
            let data = if block.ec_data_shards > 0 {
                self.read_ec_block(&block).await?
            } else {
                self.read_block_range(&block.locations, &block.block_id, 0, 0).await?
            };
            file.write_all(&data)?;
        }

        Ok(())
    }

    /// Read a specific range of bytes from a file
    /// Returns the data as Vec<u8>
    pub async fn read_file_range(
        &self,
        path: &str,
        offset: u64,
        length: u64,
    ) -> anyhow::Result<Vec<u8>> {
        // 1. Get file info from Master
        let (info_resp, _) = self
            .execute_rpc(Some(path), |mut client| {
                let path = path.to_string();
                async move {
                    let info_req = tonic::Request::new(GetFileInfoRequest { path });
                    client.get_file_info(info_req).await
                }
            })
            .await?;
        let info_resp = info_resp.into_inner();

        if !info_resp.found {
            bail!("File not found");
        }

        let metadata = info_resp.metadata.ok_or_else(|| anyhow!("No metadata"))?;

        // Validate range
        if offset >= metadata.size {
            bail!("Offset {} exceeds file size {}", offset, metadata.size);
        }

        let bytes_to_read = std::cmp::min(length, metadata.size - offset);
        let mut result = Vec::with_capacity(bytes_to_read as usize);
        let end_offset = offset + bytes_to_read;

        tracing::info!(
            "read_file_range: path={}, offset={}, length={}, bytes_to_read={}, file_size={}, blocks={}",
            path, offset, length, bytes_to_read, metadata.size, metadata.blocks.len()
        );

        // Track cumulative file position
        let mut file_position = 0u64;

        // 2. Read blocks from ChunkServers
        tracing::info!(
            "Starting block iteration: {} blocks to process",
            metadata.blocks.len()
        );
        for block in metadata.blocks {
            tracing::debug!(
                "Processing block: id={}, size={}, locations={:?}",
                block.block_id,
                block.size,
                block.locations
            );

            // Calculate this block's position in the file
            let block_start = file_position;
            let block_end = file_position + block.size;

            // Move to next block
            file_position += block.size;

            // Skip blocks completely before our range
            if block_end <= offset {
                tracing::debug!("Skipping block {} (before range)", block.block_id);
                continue;
            }

            // Stop if we've read past our range
            if block_start >= end_offset {
                tracing::debug!("Stopping at block {} (past range)", block.block_id);
                break;
            }

            // Calculate offset and length within this block
            let block_offset = offset.saturating_sub(block_start);

            let block_read_end = std::cmp::min(block.size, end_offset - block_start);
            let block_length = block_read_end - block_offset;

            tracing::info!(
                "Reading from block {}: block_start={}, block_end={}, offset={}, length={}",
                block.block_id,
                block_start,
                block_end,
                block_offset,
                block_length
            );

            let data = if block.ec_data_shards > 0 {
                // For EC blocks: read the full block and apply offset/length slice
                let full = self.read_ec_block(&block).await?;
                let start = block_offset as usize;
                let end = (block_offset + block_length) as usize;
                full[start..end.min(full.len())].to_vec()
            } else {
                self.read_block_range(&block.locations, &block.block_id, block_offset, block_length)
                    .await?
            };
            result.extend_from_slice(&data);
        }

        tracing::info!(
            "read_file_range completed: returned {} bytes (expected {})",
            result.len(),
            bytes_to_read
        );

        Ok(result)
    }

    /// Download file with concurrent block fetching for better performance
    /// This method fetches multiple blocks in parallel
    pub async fn get_file_concurrent(&self, source: &str, dest: &Path) -> anyhow::Result<()> {
        let data = self.get_file_content(source).await?;
        let mut file = File::create(dest)?;
        file.write_all(&data)?;
        Ok(())
    }

    /// Fetch file content in memory
    pub async fn get_file_content(&self, source: &str) -> anyhow::Result<Vec<u8>> {
        // 1. Get file info from Master
        let (info_resp, _) = self
            .execute_rpc(Some(source), |mut client| {
                let source = source.to_string();
                async move {
                    let info_req = tonic::Request::new(GetFileInfoRequest { path: source });
                    client.get_file_info(info_req).await
                }
            })
            .await?;
        let info_resp = info_resp.into_inner();

        if !info_resp.found {
            bail!("File not found");
        }

        let metadata = info_resp.metadata.ok_or_else(|| anyhow!("No metadata"))?;
        let blocks = metadata.blocks;

        if blocks.is_empty() {
            return Ok(Vec::new());
        }

        // 2. Fetch all blocks concurrently
        let mut fetch_tasks = Vec::new();

        for (idx, block) in blocks.iter().enumerate() {
            let block_clone = block.clone();
            let self_clone = self.clone();

            let task = tokio::spawn(async move {
                self_clone
                    .fetch_single_block(&block_clone)
                    .await
                    .map(|data| (idx, data))
            });

            fetch_tasks.push(task);
        }

        // 3. Wait for all blocks and collect results
        let mut block_data: Vec<(usize, Vec<u8>)> = Vec::new();

        for task in fetch_tasks {
            match task.await {
                Ok(Ok(data)) => block_data.push(data),
                Ok(Err(e)) => return Err(e),
                Err(e) => bail!("Task join error: {}", e),
            }
        }

        // 4. Sort by block index and join
        block_data.sort_by_key(|(idx, _)| *idx);

        let mut result = Vec::with_capacity(metadata.size as usize);
        for (_, data) in block_data {
            result.extend_from_slice(&data);
        }

        Ok(result)
    }

    /// Read a block range from a single ChunkServer location.
    /// `length = 0` means read the entire block.
    async fn read_block_from_location(
        &self,
        location: &str,
        block_id: &str,
        offset: u64,
        length: u64,
    ) -> anyhow::Result<Vec<u8>> {
        let chunk_server_addr = format!("http://{}", location);
        let channel = self.connect_endpoint(&chunk_server_addr).await?;
        let mut client = ChunkServerServiceClient::with_interceptor(
            channel,
            dfs_common::telemetry::tracing_interceptor
                as fn(tonic::Request<()>) -> Result<tonic::Request<()>, tonic::Status>,
        )
        .max_decoding_message_size(100 * 1024 * 1024);

        let request = tonic::Request::new(ReadBlockRequest {
            block_id: block_id.to_string(),
            offset,
            length,
        });
        let data = client.read_block(request).await?.into_inner().data;
        Ok(data)
    }

    /// Read a block range from the given locations, applying hedging if configured.
    /// Tries locations sequentially (or with hedge delay) until one succeeds.
    async fn read_block_range(
        &self,
        locations: &[String],
        block_id: &str,
        offset: u64,
        length: u64,
    ) -> anyhow::Result<Vec<u8>> {
        if locations.is_empty() {
            bail!("Block {} has no locations", block_id);
        }

        let hedge_delay = match self.hedge_delay_ms {
            Some(d) if locations.len() > 1 => tokio::time::Duration::from_millis(d),
            _ => {
                for location in locations {
                    match self
                        .read_block_from_location(location, block_id, offset, length)
                        .await
                    {
                        Ok(data) => return Ok(data),
                        Err(e) => tracing::warn!(
                            "Failed to read block {} from {}: {}", block_id, location, e
                        ),
                    }
                }
                bail!("Failed to read block {} from any location", block_id);
            }
        };

        // Hedged path: spawn primary, race against hedge delay.
        let mut primary_done = false;
        let self1 = self.clone();
        let bid = block_id.to_string();
        let loc0 = locations[0].clone();
        let mut primary = tokio::spawn(async move {
            self1.read_block_from_location(&loc0, &bid, offset, length).await
        });

        tokio::select! {
            result = &mut primary => {
                let r = result.unwrap_or_else(|e| Err(anyhow::anyhow!("Task error: {}", e)));
                if r.is_ok() {
                    // Primary succeeded before hedge delay — no need to hedge.
                    return r;
                }
                tracing::warn!(
                    "Primary fast-failed for block {}, launching hedge immediately",
                    block_id
                );
                // Mark primary as exhausted so we don't re-poll it below.
                primary_done = true;
            }
            _ = tokio::time::sleep(hedge_delay) => {
                tracing::debug!("Hedge triggered for block {} after {:?}", block_id, hedge_delay);
            }
        }

        // Launch hedge to the next location.
        let self2 = self.clone();
        let bid2 = block_id.to_string();
        let loc1 = locations[1].clone();
        let mut hedge = tokio::spawn(async move {
            self2.read_block_from_location(&loc1, &bid2, offset, length).await
        });

        if primary_done {
            // Primary already exhausted — only hedge can help.
            primary.abort(); // no-op since already done, but explicit
            let hr = hedge
                .await
                .unwrap_or_else(|e| Err(anyhow::anyhow!("Hedge task error: {}", e)));
            if hr.is_ok() {
                return hr;
            }
            tracing::warn!("Hedge also failed for block {}", block_id);
            // Fall through to sequential fallback below.
        } else {
            // Both are still running — race them.
            // Collect which finished first and its result, then handle the sibling OUTSIDE
            // the select! to keep this future cancellation-safe.
            enum Winner {
                Primary(anyhow::Result<Vec<u8>>),
                Hedge(anyhow::Result<Vec<u8>>),
            }

            let winner = tokio::select! {
                result = &mut primary => Winner::Primary(
                    result.unwrap_or_else(|e| Err(anyhow::anyhow!("Primary task error: {}", e)))
                ),
                result = &mut hedge => Winner::Hedge(
                    result.unwrap_or_else(|e| Err(anyhow::anyhow!("Hedge task error: {}", e)))
                ),
            };

            match winner {
                Winner::Primary(r) if r.is_ok() => {
                    hedge.abort();
                    return r;
                }
                Winner::Hedge(r) if r.is_ok() => {
                    primary.abort();
                    return r;
                }
                Winner::Primary(_) => {
                    tracing::warn!(
                        "Primary hedged read failed for block {}, waiting for hedge",
                        block_id
                    );
                    let hr = hedge
                        .await
                        .unwrap_or_else(|e| Err(anyhow::anyhow!("Hedge task error: {}", e)));
                    if hr.is_ok() {
                        return hr;
                    }
                    tracing::warn!("Hedge also failed for block {}", block_id);
                    // Fall through to sequential fallback below.
                }
                Winner::Hedge(_) => {
                    tracing::warn!(
                        "Hedge read failed for block {}, waiting for primary",
                        block_id
                    );
                    let pr = primary
                        .await
                        .unwrap_or_else(|e| Err(anyhow::anyhow!("Primary task error: {}", e)));
                    if pr.is_ok() {
                        return pr;
                    }
                    tracing::warn!("Primary also failed for block {}", block_id);
                    // Fall through to sequential fallback below.
                }
            }
        }

        // Both hedged replicas failed — fall back to remaining replicas sequentially.
        tracing::warn!(
            "Hedged read failed for block {} (first 2 replicas), trying remaining",
            block_id
        );
        for location in locations.iter().skip(2) {
            match self
                .read_block_from_location(location, block_id, offset, length)
                .await
            {
                Ok(data) => return Ok(data),
                Err(e) => tracing::warn!(
                    "Failed to read block {} from {}: {}", block_id, location, e
                ),
            }
        }

        bail!("Failed to read block {} from any location", block_id)
    }

    /// Read an EC-encoded block by fetching all k+m shards concurrently and reconstructing.
    async fn read_ec_block(&self, block: &crate::dfs::BlockInfo) -> anyhow::Result<Vec<u8>> {
        let data_shards = block.ec_data_shards as usize;
        let parity_shards = block.ec_parity_shards as usize;
        let original_size = block.original_size as usize;
        let total = data_shards + parity_shards;

        // Fetch all shards concurrently
        let mut fetch_futs = Vec::new();
        for (i, loc) in block.locations.iter().enumerate() {
            let block_id = block.block_id.clone();
            let loc = loc.clone();
            let self_clone = self.clone();
            fetch_futs.push(async move {
                let result = self_clone.read_block_from_location(&loc, &block_id, 0, 0).await;
                (i, result)
            });
        }
        let results = futures_util::future::join_all(fetch_futs).await;

        let mut opt_shards: Vec<Option<Vec<u8>>> = vec![None; total];
        let mut available = 0usize;
        for (i, result) in results {
            if i >= total { continue; }
            if let Ok(data) = result {
                opt_shards[i] = Some(data);
                available += 1;
            }
        }

        if available < data_shards {
            bail!(
                "EC block {} unrecoverable: only {}/{} shards available",
                block.block_id, available, data_shards
            );
        }

        // Fast path: all data shards present — just concatenate
        let all_data_ok = opt_shards[..data_shards].iter().all(|s| s.is_some());
        if all_data_ok {
            let mut result = Vec::new();
            for s in &opt_shards[..data_shards] {
                result.extend_from_slice(s.as_ref().unwrap());
            }
            result.truncate(original_size);
            return Ok(result);
        }

        // Degraded read: RS decode
        dfs_common::erasure::decode(&mut opt_shards, data_shards, parity_shards, original_size)
    }

    /// Helper function to fetch a single block from any available location.
    /// For EC blocks, uses the EC read path (concurrent shard fetch + RS decode if needed).
    pub async fn fetch_single_block(&self, block: &dfs::BlockInfo) -> anyhow::Result<Vec<u8>> {
        if block.locations.is_empty() {
            bail!("Block {} has no locations", block.block_id);
        }
        if block.ec_data_shards > 0 {
            self.read_ec_block(block).await
        } else {
            self.read_block_range(&block.locations, &block.block_id, 0, 0).await
        }
    }

    pub async fn exists(
        &self,
        path: &str,
    ) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
        let (info_resp, _) = self
            .execute_rpc(Some(path), |mut client| {
                let path = path.to_string();
                async move {
                    let info_req = tonic::Request::new(GetFileInfoRequest { path });
                    client.get_file_info(info_req).await
                }
            })
            .await?;
        Ok(info_resp.into_inner().found)
    }

    pub async fn delete_file(&self, path: &str) -> anyhow::Result<()> {
        let (delete_resp, _) = self
            .execute_rpc(Some(path), |mut client| {
                let path = path.to_string();
                async move {
                    let delete_req = tonic::Request::new(DeleteFileRequest { path });
                    let response = client.delete_file(delete_req).await?;
                    let inner = response.get_ref();
                    if !inner.success && inner.error_message == "Not Leader" {
                        return Err(tonic::Status::unavailable(format!(
                            "Not Leader|{}",
                            inner.leader_hint
                        )));
                    }
                    Ok(response)
                }
            })
            .await?;
        let delete_resp = delete_resp.into_inner();

        if !delete_resp.success {
            bail!("Failed to delete file: {}", delete_resp.error_message);
        }

        Ok(())
    }

    pub async fn rename_file(&self, source: &str, dest: &str) -> anyhow::Result<()> {
        // Use source path for routing
        let (rename_resp, _) = self
            .execute_rpc(Some(source), |mut client| {
                let source = source.to_string();
                let dest = dest.to_string();
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
            })
            .await?;
        let rename_resp = rename_resp.into_inner();
        if rename_resp.success {
            Ok(())
        } else {
            bail!("Failed to rename file: {}", rename_resp.error_message);
        }
    }

    pub async fn initiate_shuffle(&self, prefix: &str) -> anyhow::Result<()> {
        let (resp, _) = self
            .execute_rpc(Some(prefix), |mut client| {
                let prefix = prefix.to_string();
                async move {
                    let req = tonic::Request::new(crate::dfs::InitiateShuffleRequest { prefix });
                    let response = client.initiate_shuffle(req).await?;
                    let inner = response.get_ref();
                    if !inner.success && inner.error_message == "Not Leader" {
                        return Err(tonic::Status::unavailable(format!(
                            "Not Leader|{}",
                            inner.leader_hint
                        )));
                    }
                    if !inner.error_message.is_empty()
                        && inner.error_message.starts_with("REDIRECT:")
                    {
                        return Err(tonic::Status::out_of_range(inner.error_message.clone()));
                    }
                    Ok(response)
                }
            })
            .await?;

        if resp.into_inner().success {
            Ok(())
        } else {
            bail!("Failed to initiate shuffle")
        }
    }

    async fn execute_rpc<F, Fut, R>(&self, key: Option<&str>, f: F) -> anyhow::Result<(R, String)>
    where
        F: Fn(
            MasterServiceClient<
                tonic::service::interceptor::InterceptedService<
                    Channel,
                    fn(tonic::Request<()>) -> Result<tonic::Request<()>, tonic::Status>,
                >,
            >,
        ) -> Fut,
        Fut: std::future::Future<Output = Result<R, tonic::Status>>,
    {
        let mut initial_targets = Vec::new();
        if let Some(k) = key {
            let map = self.shard_map.read().unwrap();
            if let Some(shard_id) = map.get_shard(k) {
                if let Some(peers) = map.get_shard_peers(&shard_id) {
                    initial_targets = peers.clone();
                }
            }
        }

        if initial_targets.is_empty() {
            initial_targets = self.master_addrs.clone();
        }

        self.execute_rpc_internal(
            &initial_targets,
            self.max_retries,
            self.initial_backoff_ms,
            f,
        )
        .await
    }

    async fn execute_rpc_internal<F, Fut, T>(
        &self,
        masters: &[String],
        max_retries: usize,
        initial_backoff_ms: u64,
        f: F,
    ) -> anyhow::Result<(T, String)>
    where
        F: Fn(
            MasterServiceClient<
                tonic::service::interceptor::InterceptedService<
                    Channel,
                    fn(tonic::Request<()>) -> Result<tonic::Request<()>, tonic::Status>,
                >,
            >,
        ) -> Fut,
        Fut: std::future::Future<Output = Result<T, tonic::Status>>,
    {
        let mut attempt = 0;
        let mut backoff = Duration::from_millis(initial_backoff_ms);
        let mut leader_hint: Option<String> = None;

        loop {
            attempt += 1;

            let targets = if let Some(hint) = leader_hint.take() {
                let prefix = if self.ca_cert_path.is_some() {
                    "https://"
                } else {
                    "http://"
                };
                let hint_with_prefix = if hint.contains("://") {
                    hint
                } else {
                    format!("{}{}", prefix, hint)
                };
                tracing::info!("Using leader hint with prefix: {}", hint_with_prefix);
                let mut t = vec![hint_with_prefix];
                t.extend_from_slice(masters);
                t
            } else {
                masters.to_vec()
            };

            for master_addr in targets {
                if master_addr.is_empty() {
                    continue;
                }

                let mut addr = master_addr.clone();
                if self.ca_cert_path.is_some() && addr.starts_with("http://") {
                    addr = addr.replace("http://", "https://");
                }

                let resolved_addr = self.resolve_url(&addr);
                let mut endpoint =
                    match tonic::transport::Endpoint::from_shared(resolved_addr.clone()) {
                        Ok(endpoint) => endpoint,
                        Err(e) => {
                            tracing::error!("Invalid URL {}: {}", resolved_addr, e);
                            continue;
                        }
                    };

                if let Some(ca_path) = &self.ca_cert_path {
                    let domain = self.domain_name.clone().unwrap_or_else(|| {
                        // Extract hostname from addr for default domain
                        addr.split("://")
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
                        endpoint = endpoint
                            .tls_config(tls_config)
                            .expect("Failed to apply TLS config");
                    }
                }

                let channel = match endpoint.connect().await {
                    Ok(channel) => channel,
                    Err(e) => {
                        tracing::error!(
                            "Failed to connect to {} (resolved: {}): {}",
                            addr,
                            resolved_addr,
                            e
                        );
                        continue;
                    }
                };

                let client = MasterServiceClient::with_interceptor(
                    channel,
                    dfs_common::telemetry::tracing_interceptor
                        as fn(tonic::Request<()>) -> Result<tonic::Request<()>, tonic::Status>,
                );
                let client = client.max_decoding_message_size(100 * 1024 * 1024);

                match f(client).await {
                    Ok(res) => return Ok((res, master_addr)),
                    Err(status) => {
                        let msg = status.message();
                        tracing::info!(
                            "RPC to {} failed: code={:?}, message={}",
                            master_addr,
                            status.code(),
                            msg
                        );
                        if msg.starts_with("REDIRECT:") {
                            let parts: Vec<&str> = msg.splitn(2, ':').collect();
                            if parts.len() > 1 && !parts[1].is_empty() {
                                leader_hint = Some(parts[1].to_string());
                                tracing::info!(
                                    "Received SHARD REDIRECT to: {}. Refreshing ShardMap...",
                                    parts[1]
                                );

                                // Refresh shard map in background or inline
                                let self_clone = self.clone();
                                tokio::spawn(async move {
                                    let _ = self_clone.refresh_shard_map().await;
                                });
                                break;
                            }
                        }

                        if msg.starts_with("Not Leader|") {
                            let parts: Vec<&str> = msg.split('|').collect();
                            if parts.len() > 1 && !parts[1].is_empty() {
                                leader_hint = Some(parts[1].to_string());
                                tracing::info!("Received leader hint: {}", parts[1]);
                                break;
                            }
                        }

                        if msg.contains("Not Leader") || status.code() == tonic::Code::Unavailable {
                            continue;
                        }
                        return Err(status.into());
                    }
                }
            }

            if attempt >= max_retries {
                break;
            }

            if leader_hint.is_some() {
                tracing::info!("Retrying with leader hint...");
            } else {
                tracing::info!("No leader found, retrying in {:?}...", backoff);
                sleep(backoff).await;
                backoff = std::cmp::min(backoff * 2, Duration::from_secs(5));
            }
        }

        bail!("No available leader found after retries")
    }

    pub async fn refresh_shard_map(&self) -> anyhow::Result<()> {
        if self.config_server_addrs.is_empty() {
            return Ok(());
        }

        use crate::dfs::config_service_client::ConfigServiceClient;
        use crate::dfs::FetchShardMapRequest;

        for addr in &self.config_server_addrs {
            let config_addr_with_prefix = if addr.starts_with("http://") {
                addr.clone()
            } else {
                format!("http://{}", addr)
            };
            let resolved_addr = self.resolve_url(&config_addr_with_prefix);
            if let Ok(mut client) = ConfigServiceClient::connect(resolved_addr).await {
                if let Ok(resp) = client.fetch_shard_map(FetchShardMapRequest {}).await {
                    let shards_data = resp.into_inner().shards;
                    let mut shards_vec: Vec<_> = shards_data.into_iter().collect();
                    shards_vec.sort_by(|a, b| a.0.cmp(&b.0));

                    let mut new_map = ShardMap::new_range(); // Assume range strategy for dynamic sharding

                    for (shard_id, peers_info) in shards_vec {
                        new_map.add_shard(shard_id, peers_info.peers);
                    }

                    // Note: We might need a better way to recover the exact split points
                    // if they are not returned by fetch_shard_map.
                    // For now, assume fetch_shard_map returns the correct boundaries if updated.

                    self.set_shard_map(new_map);
                    tracing::info!(
                        "Successfully refreshed ShardMap from Config server {}",
                        addr
                    );
                    return Ok(());
                }
            }
        }
        bail!("Failed to refresh ShardMap from any Config server")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ec_encode_produces_correct_shard_count() {
        let data: Vec<u8> = (0..1024u16).map(|i| (i % 256) as u8).collect();
        let shards = dfs_common::erasure::encode(&data, 4, 2).unwrap();
        assert_eq!(shards.len(), 6);
        // Each shard should be ceil(1024/4) = 256 bytes
        for s in &shards {
            assert_eq!(s.len(), 256);
        }
    }

    #[test]
    fn test_ec_shard_size_calculation() {
        assert_eq!(dfs_common::erasure::shard_len(1000, 4), 250);
        assert_eq!(dfs_common::erasure::shard_len(1001, 4), 251); // ceiling
        assert_eq!(dfs_common::erasure::shard_len(4, 4), 1);
    }

    fn make_client() -> Client {
        Client::new(vec!["http://localhost:50051".to_string()], vec![])
    }

    #[tokio::test]
    async fn test_read_block_from_location_unreachable_returns_error() {
        let client = make_client();
        let result = client
            .read_block_from_location("localhost:19999", "block-x", 0, 0)
            .await;
        assert!(result.is_err(), "Expected error for unreachable server");
    }

    #[tokio::test]
    async fn test_fetch_single_block_empty_locations_returns_error() {
        let client = make_client();
        let block = crate::dfs::BlockInfo {
            block_id: "b1".to_string(),
            size: 0,
            locations: vec![],
            checksum_crc32c: 0,
            ec_data_shards: 0,
            ec_parity_shards: 0,
            original_size: 0,
        };
        let result = client.fetch_single_block(&block).await;
        assert!(result.is_err());
        let msg = format!("{}", result.unwrap_err());
        assert!(msg.contains("no locations"), "Got: {}", msg);
    }

    #[test]
    fn test_client_hedge_delay_default_is_none() {
        let client = make_client();
        assert!(client.hedge_delay_ms.is_none());
    }

    #[test]
    fn test_client_with_hedge_delay_sets_field() {
        let client = make_client().with_hedge_delay(50);
        assert_eq!(client.hedge_delay_ms, Some(50));
    }

    #[tokio::test]
    async fn test_fetch_single_block_no_hedge_when_one_location() {
        // With hedge enabled but only 1 location, should still work (no panic)
        let client = make_client().with_hedge_delay(1);
        let block = crate::dfs::BlockInfo {
            block_id: "b1".to_string(),
            size: 0,
            locations: vec!["localhost:19999".to_string()], // unreachable
            checksum_crc32c: 0,
            ec_data_shards: 0,
            ec_parity_shards: 0,
            original_size: 0,
        };
        // Should fail gracefully (not panic) even with hedge enabled
        let result = client.fetch_single_block(&block).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_fetch_single_block_hedge_disabled_with_two_locations() {
        // Hedge disabled: tries locations sequentially, both unreachable → error
        let client = make_client(); // no hedge
        let block = crate::dfs::BlockInfo {
            block_id: "b2".to_string(),
            size: 0,
            locations: vec![
                "localhost:19997".to_string(),
                "localhost:19998".to_string(),
            ],
            checksum_crc32c: 0,
            ec_data_shards: 0,
            ec_parity_shards: 0,
            original_size: 0,
        };
        let result = client.fetch_single_block(&block).await;
        assert!(result.is_err());
    }

    #[test]
    fn test_ec_read_normal_path_no_decode_needed() {
        // All 4 data shards available — just concatenate
        let original = vec![42u8; 5000];
        let shards = dfs_common::erasure::encode(&original, 4, 2).unwrap();

        // Simulate: all data shards present
        let mut combined = Vec::new();
        for s in &shards[..4] {
            combined.extend_from_slice(s);
        }
        combined.truncate(5000);
        assert_eq!(combined, original);
    }

    #[test]
    fn test_ec_read_degraded_with_two_missing_shards() {
        let original = vec![99u8; 3333];
        let shards = dfs_common::erasure::encode(&original, 4, 2).unwrap();

        // Shards 1 and 3 unavailable
        let mut opt: Vec<Option<Vec<u8>>> = shards.iter().map(|s| Some(s.clone())).collect();
        opt[1] = None;
        opt[3] = None;

        let recovered = dfs_common::erasure::decode(&mut opt, 4, 2, original.len()).unwrap();
        assert_eq!(recovered, original);
    }
}
