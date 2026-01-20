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
        }
    }

    pub fn with_retry_config(mut self, max_retries: usize, initial_backoff_ms: u64) -> Self {
        self.max_retries = max_retries;
        self.initial_backoff_ms = initial_backoff_ms;
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
        // 1. Create file on Master
        let (create_resp, success_addr) = self
            .execute_rpc(Some(dest), |mut client| {
                let dest = dest.to_string();
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
            })
            .await?;
        let create_resp = create_resp.into_inner();

        if !create_resp.success {
            bail!("Failed to create file: {}", create_resp.error_message);
        }

        // 2. Read local file
        let mut file = File::open(source)?;
        let mut buffer = Vec::new();
        file.read_to_end(&mut buffer)?;

        // 3. Allocate block
        // Use the master that handled create_file successfully to ensure read-your-writes consistency
        let buffer_len = buffer.len() as u64;
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

        // 4. Write to first chunk server with replication pipeline
        let chunk_server_addr = format!("http://{}", chunk_servers[0]);
        let resolved_addr = self.resolve_url(&chunk_server_addr);
        let channel = tonic::transport::Endpoint::from_shared(resolved_addr.clone())?
            .connect()
            .await?;
        let mut chunk_client =
            crate::dfs::chunk_server_service_client::ChunkServerServiceClient::with_interceptor(
                channel,
                dfs_common::telemetry::tracing_interceptor
                    as fn(tonic::Request<()>) -> Result<tonic::Request<()>, tonic::Status>,
            )
            .max_decoding_message_size(100 * 1024 * 1024);

        let next_servers = chunk_servers[1..].to_vec();

        let write_req = tonic::Request::new(WriteBlockRequest {
            block_id: block.block_id,
            data: buffer,
            next_servers,
        });

        let write_resp = chunk_client.write_block(write_req).await?.into_inner();
        if !write_resp.success {
            bail!("Failed to write block: {}", write_resp.error_message);
        }

        // 5. Complete file
        let (complete_resp, _) = self
            .execute_rpc(Some(dest), |mut client| {
                let dest = dest.to_string();
                let size = buffer_len;
                async move {
                    let complete_req =
                        tonic::Request::new(CompleteFileRequest { path: dest, size });
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
            if block.locations.is_empty() {
                tracing::warn!("Block {} has no locations", block.block_id);
                continue;
            }

            let mut success = false;
            for location in block.locations {
                let chunk_server_addr = format!("http://{}", location);
                let resolved_addr = self.resolve_url(&chunk_server_addr);
                let channel_res = tonic::transport::Endpoint::from_shared(resolved_addr.clone());
                match channel_res {
                    Ok(endpoint) => match endpoint.connect().await {
                        Ok(channel) => {
                            let mut client = ChunkServerServiceClient::with_interceptor(
                                channel,
                                dfs_common::telemetry::tracing_interceptor
                                    as fn(
                                        tonic::Request<()>,
                                    )
                                        -> Result<tonic::Request<()>, tonic::Status>,
                            )
                            .max_decoding_message_size(100 * 1024 * 1024);
                            let request = tonic::Request::new(ReadBlockRequest {
                                block_id: block.block_id.clone(),
                            });
                            match client.read_block(request).await {
                                Ok(response) => {
                                    let data = response.into_inner().data;
                                    file.write_all(&data)?;
                                    success = true;
                                    break;
                                }
                                Err(e) => {
                                    tracing::error!("Failed to read block from {}: {}", location, e)
                                }
                            }
                        }
                        Err(e) => tracing::error!("Failed to connect to {}: {}", location, e),
                    },
                    Err(e) => tracing::error!("Invalid URL {}: {}", resolved_addr, e),
                }
            }

            if !success {
                bail!("Failed to read block {} from any location", block.block_id);
            }
        }

        Ok(())
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
                let hint_with_prefix = if hint.starts_with("http://") {
                    hint
                } else {
                    format!("http://{}", hint)
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

                let resolved_addr = self.resolve_url(&master_addr);
                let channel = match tonic::transport::Endpoint::from_shared(resolved_addr.clone()) {
                    Ok(endpoint) => match endpoint.connect().await {
                        Ok(channel) => channel,
                        Err(e) => {
                            tracing::error!(
                                "Failed to connect to {} (resolved: {}): {}",
                                master_addr,
                                resolved_addr,
                                e
                            );
                            continue;
                        }
                    },
                    Err(e) => {
                        tracing::error!("Invalid URL {}: {}", resolved_addr, e);
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
