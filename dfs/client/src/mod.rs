pub mod dfs {
    include!(concat!(env!("OUT_DIR"), "/dfs.rs"));
}

pub mod sharding;

use crate::dfs::chunk_server_service_client::ChunkServerServiceClient;
use crate::dfs::master_service_client::MasterServiceClient;
use crate::dfs::{
    AllocateBlockRequest, CreateFileRequest, DeleteFileRequest, GetFileInfoRequest,
    ListFilesRequest, ReadBlockRequest, RenameRequest, WriteBlockRequest,
};
use crate::sharding::ShardMap;
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
    max_retries: usize,
    initial_backoff_ms: u64,
}

impl Client {
    pub fn new(master_addrs: Vec<String>) -> Self {
        // Initialize with default (empty) ShardMap.
        // In a real scenario, we would fetch this from a Config Server.
        Self {
            master_addrs,
            shard_map: Arc::new(RwLock::new(ShardMap::new(100))),
            host_aliases: Arc::new(RwLock::new(HashMap::new())),
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

    pub async fn list_files(
        &self,
        path: &str,
    ) -> Result<Vec<String>, Box<dyn std::error::Error + Send + Sync>> {
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

    pub async fn list_all_files(
        &self,
    ) -> Result<Vec<String>, Box<dyn std::error::Error + Send + Sync>> {
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
                        eprintln!("Failed to list files from shard {}: {}", shard_id, e);
                        return Err(e);
                    }
                }
            }
        }

        Ok(all_files.into_iter().collect())
    }

    pub async fn create_file(
        &self,
        source: &Path,
        dest: &str,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
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
            return Err(format!("Failed to create file: {}", create_resp.error_message).into());
        }

        // 2. Read local file
        let mut file = File::open(source)?;
        let mut buffer = Vec::new();
        file.read_to_end(&mut buffer)?;

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

        let block = alloc_resp.block.ok_or("No block allocated")?;
        let chunk_servers = alloc_resp.chunk_server_addresses;

        if chunk_servers.is_empty() {
            return Err("No chunk servers available".into());
        }

        println!(
            "Replicating to {} servers: {:?}",
            chunk_servers.len(),
            chunk_servers
        );

        // 4. Write to first chunk server with replication pipeline
        let chunk_server_addr = format!("http://{}", chunk_servers[0]);
        let resolved_addr = self.resolve_url(&chunk_server_addr);
        let mut chunk_client = ChunkServerServiceClient::connect(resolved_addr)
            .await?
            .max_decoding_message_size(100 * 1024 * 1024);

        let next_servers = chunk_servers[1..].to_vec();

        let write_req = tonic::Request::new(WriteBlockRequest {
            block_id: block.block_id,
            data: buffer,
            next_servers,
        });

        let write_resp = chunk_client.write_block(write_req).await?.into_inner();
        if !write_resp.success {
            return Err(format!("Failed to write block: {}", write_resp.error_message).into());
        }

        Ok(())
    }

    pub async fn get_file(
        &self,
        source: &str,
        dest: &Path,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
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
            return Err("File not found".into());
        }

        let metadata = info_resp.metadata.ok_or("No metadata")?;
        let mut file = File::create(dest)?;

        // 2. Read blocks from ChunkServers
        for block in metadata.blocks {
            if block.locations.is_empty() {
                eprintln!("Block {} has no locations", block.block_id);
                continue;
            }

            let mut success = false;
            for location in block.locations {
                let chunk_server_addr = format!("http://{}", location);
                let resolved_addr = self.resolve_url(&chunk_server_addr);
                match ChunkServerServiceClient::connect(resolved_addr).await {
                    Ok(client) => {
                        let mut chunk_client = client.max_decoding_message_size(100 * 1024 * 1024);
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
                            Err(e) => eprintln!("Failed to read block from {}: {}", location, e),
                        }
                    }
                    Err(e) => eprintln!("Failed to connect to {}: {}", location, e),
                }
            }

            if !success {
                return Err(format!("Failed to read block {}", block.block_id).into());
            }
        }

        Ok(())
    }

    pub async fn delete_file(
        &self,
        path: &str,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
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
            return Err(format!("Failed to delete file: {}", delete_resp.error_message).into());
        }

        Ok(())
    }

    pub async fn rename_file(
        &self,
        source: &str,
        dest: &str,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
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
            Err(format!("Failed to rename file: {}", rename_resp.error_message).into())
        }
    }

    async fn execute_rpc<F, Fut, T>(
        &self,
        key: Option<&str>,
        f: F,
    ) -> Result<(T, String), Box<dyn std::error::Error + Send + Sync>>
    where
        F: Fn(MasterServiceClient<Channel>) -> Fut,
        Fut: std::future::Future<Output = Result<T, tonic::Status>>,
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
    ) -> Result<(T, String), Box<dyn std::error::Error + Send + Sync>>
    where
        F: Fn(MasterServiceClient<Channel>) -> Fut,
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
                eprintln!("Using leader hint with prefix: {}", hint_with_prefix);
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
                let client = match MasterServiceClient::connect(resolved_addr.clone()).await {
                    Ok(c) => c,
                    Err(e) => {
                        eprintln!(
                            "Failed to connect to {} (resolved: {}): {}",
                            master_addr, resolved_addr, e
                        );
                        continue;
                    }
                };
                let client = client.max_decoding_message_size(100 * 1024 * 1024);

                match f(client).await {
                    Ok(res) => return Ok((res, master_addr)),
                    Err(status) => {
                        let msg = status.message();
                        if msg.starts_with("REDIRECT:") {
                            let parts: Vec<&str> = msg.splitn(2, ':').collect();
                            if parts.len() > 1 && !parts[1].is_empty() {
                                leader_hint = Some(parts[1].to_string());
                                eprintln!("Received SHARD REDIRECT to: {}", parts[1]);
                                break;
                            }
                        }

                        if msg.starts_with("Not Leader|") {
                            let parts: Vec<&str> = msg.split('|').collect();
                            if parts.len() > 1 && !parts[1].is_empty() {
                                leader_hint = Some(parts[1].to_string());
                                eprintln!("Received leader hint: {}", parts[1]);
                                break;
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
}
