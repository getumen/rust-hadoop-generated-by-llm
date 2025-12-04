use crate::dfs::master_service_server::MasterService;
use crate::dfs::{
    AllocateBlockRequest, AllocateBlockResponse, BlockInfo, ChunkServerCommand,
    CompleteFileRequest, CompleteFileResponse, CreateFileRequest, CreateFileResponse, FileMetadata,
    GetBlockLocationsRequest, GetBlockLocationsResponse, GetFileInfoRequest, GetFileInfoResponse,
    HeartbeatRequest, HeartbeatResponse, ListFilesRequest, ListFilesResponse,
    RegisterChunkServerRequest, RegisterChunkServerResponse,
};
use crate::simple_raft::{AppState, Command, Event, MasterCommand};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;
use tonic::{Request, Response, Status};
use uuid::Uuid;

// ============================================================================
// Transaction Record Types for Cross-Shard Operations
// ============================================================================

/// Transaction state for cross-shard operations (Google Spanner style)
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TxState {
    /// Transaction started but not yet prepared
    Pending,
    /// All participants have validated and are ready to commit
    Prepared,
    /// Transaction has been committed successfully
    Committed,
    /// Transaction has been aborted
    Aborted,
}

/// Type of cross-shard transaction
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum TransactionType {
    /// Rename operation (cross-shard file move)
    Rename {
        source_path: String,
        dest_path: String,
    },
}

/// Individual operation within a transaction
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TxOperation {
    /// Target shard ID for this operation
    pub shard_id: String,
    /// Operation type
    pub op_type: TxOpType,
}

/// Type of operation
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum TxOpType {
    /// Delete a file
    Delete { path: String },
    /// Create a file with metadata
    Create {
        path: String,
        metadata: FileMetadata,
    },
}

/// Transaction Record for tracking cross-shard operations
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransactionRecord {
    /// Unique transaction ID (UUID)
    pub tx_id: String,
    /// Type of transaction
    pub tx_type: TransactionType,
    /// Current state of the transaction
    pub state: TxState,
    /// Timestamp when transaction started (millis since epoch)
    pub timestamp: u64,
    /// List of participating shard IDs
    pub participants: Vec<String>,
    /// Operations to be performed on each shard
    pub operations: Vec<TxOperation>,
    /// Client ID that initiated the transaction (for rate limiting)
    pub client_id: String,
}

impl TransactionRecord {
    /// Create a new transaction record for a rename operation
    pub fn new_rename(
        tx_id: String,
        source_path: String,
        dest_path: String,
        source_shard: String,
        dest_shard: String,
        source_metadata: FileMetadata,
        client_id: String,
    ) -> Self {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;

        TransactionRecord {
            tx_id,
            tx_type: TransactionType::Rename {
                source_path: source_path.clone(),
                dest_path: dest_path.clone(),
            },
            state: TxState::Pending,
            timestamp: now,
            participants: vec![source_shard.clone(), dest_shard.clone()],
            operations: vec![
                TxOperation {
                    shard_id: source_shard,
                    op_type: TxOpType::Delete { path: source_path },
                },
                TxOperation {
                    shard_id: dest_shard,
                    op_type: TxOpType::Create {
                        path: dest_path,
                        metadata: source_metadata,
                    },
                },
            ],
            client_id,
        }
    }

    /// Check if this transaction deletes the specified file
    pub fn deletes_file(&self, path: &str) -> bool {
        self.operations
            .iter()
            .any(|op| matches!(&op.op_type, TxOpType::Delete { path: p } if p == path))
    }

    /// Check if this transaction creates the specified file
    pub fn creates_file(&self, path: &str) -> bool {
        self.operations
            .iter()
            .any(|op| matches!(&op.op_type, TxOpType::Create { path: p, .. } if p == path))
    }

    /// Get metadata for a file created by this transaction
    pub fn get_created_metadata(&self, path: &str) -> Option<FileMetadata> {
        for op in &self.operations {
            if let TxOpType::Create { path: p, metadata } = &op.op_type {
                if p == path {
                    return Some(metadata.clone());
                }
            }
        }
        None
    }

    /// Check if transaction has timed out (10 second timeout)
    pub fn is_timed_out(&self) -> bool {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        now - self.timestamp > 10_000 // 10 seconds
    }

    /// Check if transaction is stale and can be garbage collected (1 hour)
    pub fn is_stale(&self) -> bool {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        now - self.timestamp > 3_600_000 // 1 hour
    }
}

// ============================================================================
// Rate Limiting for DDoS Protection
// ============================================================================

/// Rate limit state for a single client
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ClientRateLimit {
    /// Timestamps of recent requests (within the rate limit window)
    pub request_timestamps: Vec<u64>,
}

/// Rate limiting configuration and state
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RateLimitState {
    /// Rate limit per client: client_id -> rate limit state
    pub clients: HashMap<String, ClientRateLimit>,
    /// Maximum requests per minute per client
    pub max_requests_per_minute: u32,
}

impl RateLimitState {
    pub fn new(max_requests_per_minute: u32) -> Self {
        RateLimitState {
            clients: HashMap::new(),
            max_requests_per_minute,
        }
    }

    /// Check if a client is rate limited. Returns true if request should be rejected.
    pub fn check_rate_limit(&mut self, client_id: &str) -> bool {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;

        let one_minute_ago = now.saturating_sub(60_000);

        let rate_limit = self.clients.entry(client_id.to_string()).or_default();

        // Remove old timestamps outside the window
        rate_limit
            .request_timestamps
            .retain(|&ts| ts > one_minute_ago);

        // Check if limit exceeded
        if rate_limit.request_timestamps.len() >= self.max_requests_per_minute as usize {
            return true; // Rate limited
        }

        // Record this request
        rate_limit.request_timestamps.push(now);
        false // Not rate limited
    }

    /// Clean up old rate limit entries (for clients that haven't made requests recently)
    pub fn cleanup(&mut self) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;

        let one_minute_ago = now.saturating_sub(60_000);

        self.clients.retain(|_, rate_limit| {
            rate_limit
                .request_timestamps
                .retain(|&ts| ts > one_minute_ago);
            !rate_limit.request_timestamps.is_empty()
        });
    }
}

// ============================================================================
// ChunkServer and Master State
// ============================================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChunkServerStatus {
    pub last_heartbeat: u64,
    pub used_space: u64,
    pub available_space: u64,
    pub chunk_count: u64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct MasterState {
    /// File metadata storage: path -> metadata
    pub files: HashMap<String, FileMetadata>,

    /// Transaction records for cross-shard operations: tx_id -> record
    pub transaction_records: HashMap<String, TransactionRecord>,

    /// Rate limiting state for DDoS protection
    #[serde(default)]
    pub rate_limit_state: RateLimitState,

    /// ChunkServer status (not persisted via Raft, local state only)
    #[serde(skip)]
    pub chunk_servers: HashMap<String, ChunkServerStatus>, // address -> status

    /// Pending commands for ChunkServers (not persisted via Raft)
    #[serde(skip)]
    pub pending_commands: HashMap<String, Vec<ChunkServerCommand>>, // address -> commands
}

use crate::sharding::{ShardId, ShardMap};

#[derive(Debug)]
pub struct MyMaster {
    state: Arc<Mutex<AppState>>,
    raft_tx: mpsc::Sender<Event>,
    shard_map: Arc<Mutex<ShardMap>>,
    shard_id: ShardId,
}

impl MyMaster {
    pub fn new(
        state: Arc<Mutex<AppState>>,
        raft_tx: mpsc::Sender<Event>,
        shard_map: Arc<Mutex<ShardMap>>,
        shard_id: ShardId,
    ) -> Self {
        // Spawn liveness check loop
        let state_clone = state.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(5));
            loop {
                interval.tick().await;
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_millis() as u64;

                let mut state_lock = state_clone.lock().unwrap();
                if let AppState::Master(ref mut state) = *state_lock {
                    // Remove chunk servers that haven't sent heartbeat in 15 seconds
                    let dead_servers: Vec<String> = state
                        .chunk_servers
                        .iter()
                        .filter(|(_, status)| now - status.last_heartbeat > 15000)
                        .map(|(addr, _)| addr.clone())
                        .collect();

                    for addr in dead_servers {
                        println!("ChunkServer {} is dead (no heartbeat), removing...", addr);
                        state.chunk_servers.remove(&addr);
                        state.pending_commands.remove(&addr);
                    }
                }
            }
        });

        // Spawn balancer task
        let state_clone_balancer = state.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(30)); // Check every 30s
            loop {
                interval.tick().await;
                let mut state_lock = state_clone_balancer.lock().unwrap();
                if let AppState::Master(ref mut state) = *state_lock {
                    let servers: Vec<(String, u64)> = state
                        .chunk_servers
                        .iter()
                        .map(|(addr, status)| (addr.clone(), status.available_space))
                        .collect();

                    if servers.len() < 2 {
                        continue;
                    }

                    let mut sorted_servers = servers;
                    sorted_servers.sort_by(|a, b| a.1.cmp(&b.1)); // Ascending available space (Least available first)

                    let (most_full_addr, min_avail) = sorted_servers.first().unwrap();
                    let (least_full_addr, max_avail) = sorted_servers.last().unwrap();

                    // If difference is greater than 100MB (arbitrary threshold for demo)
                    // In real world, use percentage or standard deviation
                    if max_avail > min_avail && (max_avail - min_avail) > 100 * 1024 * 1024 {
                        println!(
                            "Balancer: Detected imbalance. Moving block from {} to {}",
                            most_full_addr, least_full_addr
                        );

                        // Find a block on the most full server to move
                        let mut block_to_move = None;
                        'outer: for file in state.files.values() {
                            for block in &file.blocks {
                                if block.locations.contains(most_full_addr)
                                    && !block.locations.contains(least_full_addr)
                                {
                                    block_to_move = Some(block.block_id.clone());
                                    break 'outer;
                                }
                            }
                        }

                        if let Some(block_id) = block_to_move {
                            let command = ChunkServerCommand {
                                r#type: 1, // REPLICATE
                                block_id: block_id.clone(),
                                target_chunk_server_address: least_full_addr.clone(),
                            };

                            state
                                .pending_commands
                                .entry(most_full_addr.clone())
                                .or_default()
                                .push(command);

                            println!(
                                "Balancer: Scheduled replication of block {} from {} to {}",
                                block_id, most_full_addr, least_full_addr
                            );
                        }
                    }
                }
            }
        });

        MyMaster {
            state,
            raft_tx,
            shard_map,
            shard_id,
        }
    }

    fn check_shard_ownership(&self, path: &str) -> Result<(), Status> {
        let map = self.shard_map.lock().unwrap();
        if let Some(target_shard) = map.get_shard(path) {
            if target_shard != self.shard_id {
                // Not my shard
                // Get a hint for the target shard (e.g., first peer)
                // In a real system, we might want to know the leader of that shard.
                // For now, just return the first peer of the target shard.
                let target_peers = map.get_shard_peers(&target_shard).unwrap_or_default();
                let hint = target_peers.first().cloned().unwrap_or_default();

                // We use AlreadyExists code for Redirect to distinguish from other errors for now,
                // or we can use a custom error string format.
                // Let's use `Status::out_of_range` with "REDIRECT:<hint>"
                return Err(Status::out_of_range(format!("REDIRECT:{}", hint)));
            }
        }
        Ok(())
    }
}

#[tonic::async_trait]
impl MasterService for MyMaster {
    // ... (existing methods) ...
    async fn get_file_info(
        &self,
        request: Request<GetFileInfoRequest>,
    ) -> Result<Response<GetFileInfoResponse>, Status> {
        let req = request.into_inner();

        self.check_shard_ownership(&req.path)?;

        let state_lock = self.state.lock().unwrap();
        if let AppState::Master(ref state) = *state_lock {
            if let Some(metadata) = state.files.get(&req.path) {
                Ok(Response::new(GetFileInfoResponse {
                    metadata: Some(metadata.clone()),
                    found: true,
                }))
            } else {
                Ok(Response::new(GetFileInfoResponse {
                    metadata: None,
                    found: false,
                }))
            }
        } else {
            Err(Status::internal("Wrong state type"))
        }
    }

    async fn create_file(
        &self,
        request: Request<CreateFileRequest>,
    ) -> Result<Response<CreateFileResponse>, Status> {
        let req = request.into_inner();

        // Check if file exists (read optimization)
        {
            let state_lock = self.state.lock().unwrap();
            if let AppState::Master(ref state) = *state_lock {
                if state.files.contains_key(&req.path) {
                    return Ok(Response::new(CreateFileResponse {
                        success: false,
                        error_message: "File already exists".to_string(),
                        leader_hint: "".to_string(),
                    }));
                }
            }
        }

        let (tx, rx) = tokio::sync::oneshot::channel();
        if self
            .raft_tx
            .send(Event::ClientRequest {
                command: Command::Master(MasterCommand::CreateFile { path: req.path }),
                reply_tx: tx,
            })
            .await
            .is_err()
        {
            return Err(Status::internal("Raft channel closed"));
        }

        match rx.await {
            Ok(Ok(())) => Ok(Response::new(CreateFileResponse {
                success: true,
                error_message: "".to_string(),
                leader_hint: "".to_string(),
            })),
            Ok(Err(leader_opt)) => Ok(Response::new(CreateFileResponse {
                success: false,
                error_message: "Not Leader".to_string(),
                leader_hint: leader_opt.unwrap_or_default(),
            })),
            Err(_) => Err(Status::internal("Raft response error")),
        }
    }

    async fn allocate_block(
        &self,
        request: Request<AllocateBlockRequest>,
    ) -> Result<Response<AllocateBlockResponse>, Status> {
        let req = request.into_inner();

        // Replication factor (default: 3)
        const REPLICATION_FACTOR: usize = 3;

        let (chunk_servers, block_id) = {
            let state_lock = self.state.lock().unwrap();
            if let AppState::Master(ref state) = *state_lock {
                if !state.files.contains_key(&req.path) {
                    return Err(Status::not_found("File not found"));
                }

                // Load balancing: Select chunk servers with most available space
                let mut candidates: Vec<(String, u64)> = state
                    .chunk_servers
                    .iter()
                    .map(|(addr, status)| (addr.clone(), status.available_space))
                    .collect();

                if candidates.is_empty() {
                    return Err(Status::unavailable("No chunk servers available"));
                }

                // Sort by available space descending
                candidates.sort_by(|a, b| b.1.cmp(&a.1));

                let chunk_servers: Vec<String> =
                    candidates.into_iter().map(|(addr, _)| addr).collect();
                (chunk_servers, Uuid::new_v4().to_string())
            } else {
                return Err(Status::internal("Wrong state type"));
            }
        };

        // Select chunk servers
        let num_replicas = std::cmp::min(REPLICATION_FACTOR, chunk_servers.len());
        let selected_servers: Vec<String> =
            chunk_servers.iter().take(num_replicas).cloned().collect();

        let (tx, rx) = tokio::sync::oneshot::channel();
        if self
            .raft_tx
            .send(Event::ClientRequest {
                command: Command::Master(MasterCommand::AllocateBlock {
                    path: req.path,
                    block_id: block_id.clone(),
                    locations: selected_servers.clone(),
                }),
                reply_tx: tx,
            })
            .await
            .is_err()
        {
            return Err(Status::internal("Raft channel closed"));
        }

        match rx.await {
            Ok(Ok(())) => {
                let block = BlockInfo {
                    block_id,
                    size: 0,
                    locations: selected_servers.clone(),
                };
                Ok(Response::new(AllocateBlockResponse {
                    block: Some(block),
                    chunk_server_addresses: selected_servers,
                    leader_hint: "".to_string(),
                }))
            }
            Ok(Err(leader_opt)) => {
                let leader_hint = leader_opt.unwrap_or_default();

                Ok(Response::new(AllocateBlockResponse {
                    block: None,
                    chunk_server_addresses: vec![],
                    leader_hint,
                }))
            }
            Err(_) => Err(Status::internal("Raft response error")),
        }
    }

    async fn complete_file(
        &self,
        _request: Request<CompleteFileRequest>,
    ) -> Result<Response<CompleteFileResponse>, Status> {
        Ok(Response::new(CompleteFileResponse { success: true }))
    }

    async fn list_files(
        &self,
        _request: Request<ListFilesRequest>,
    ) -> Result<Response<ListFilesResponse>, Status> {
        let state_lock = self.state.lock().unwrap();
        if let AppState::Master(ref state) = *state_lock {
            let files: Vec<String> = state.files.keys().cloned().collect();
            Ok(Response::new(ListFilesResponse { files }))
        } else {
            Err(Status::internal("Wrong state type"))
        }
    }

    async fn register_chunk_server(
        &self,
        request: Request<RegisterChunkServerRequest>,
    ) -> Result<Response<RegisterChunkServerResponse>, Status> {
        let req = request.into_inner();

        let mut state_lock = self.state.lock().unwrap();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;

        if let AppState::Master(ref mut state) = *state_lock {
            // Initial registration with default stats or provided capacity
            state.chunk_servers.insert(
                req.address,
                ChunkServerStatus {
                    last_heartbeat: now,
                    used_space: 0,
                    available_space: req.capacity,
                    chunk_count: 0,
                },
            );
        }

        Ok(Response::new(RegisterChunkServerResponse { success: true }))
    }

    async fn heartbeat(
        &self,
        request: Request<HeartbeatRequest>,
    ) -> Result<Response<HeartbeatResponse>, Status> {
        let req = request.into_inner();
        let mut state_lock = self.state.lock().unwrap();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;

        if let AppState::Master(ref mut state) = *state_lock {
            state.chunk_servers.insert(
                req.chunk_server_address.clone(),
                ChunkServerStatus {
                    last_heartbeat: now,
                    used_space: req.used_space,
                    available_space: req.available_space,
                    chunk_count: req.chunk_count,
                },
            );

            // Retrieve pending commands
            let commands = state
                .pending_commands
                .remove(&req.chunk_server_address)
                .unwrap_or_default();

            Ok(Response::new(HeartbeatResponse {
                success: true,
                commands,
            }))
        } else {
            Ok(Response::new(HeartbeatResponse {
                success: false,
                commands: vec![],
            }))
        }
    }

    async fn get_block_locations(
        &self,
        request: Request<GetBlockLocationsRequest>,
    ) -> Result<Response<GetBlockLocationsResponse>, Status> {
        let req = request.into_inner();
        let state_lock = self.state.lock().unwrap();
        if let AppState::Master(ref state) = *state_lock {
            // Search for the block in all files
            for file_metadata in state.files.values() {
                for block in &file_metadata.blocks {
                    if block.block_id == req.block_id {
                        return Ok(Response::new(GetBlockLocationsResponse {
                            locations: block.locations.clone(),
                            found: true,
                        }));
                    }
                }
            }
        }

        // Block not found
        Ok(Response::new(GetBlockLocationsResponse {
            locations: vec![],
            found: false,
        }))
    }
}
