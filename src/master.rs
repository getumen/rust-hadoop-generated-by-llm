use crate::dfs::master_service_server::MasterService;
use crate::dfs::{
    AbortTransactionRequest, AbortTransactionResponse, AllocateBlockRequest, AllocateBlockResponse,
    BlockInfo, ChunkServerCommand, CommitTransactionRequest, CommitTransactionResponse,
    CompleteFileRequest, CompleteFileResponse, CreateFileRequest, CreateFileResponse, FileMetadata,
    GetBlockLocationsRequest, GetBlockLocationsResponse, GetFileInfoRequest, GetFileInfoResponse,
    HeartbeatRequest, HeartbeatResponse, ListFilesRequest, ListFilesResponse,
    PrepareTransactionRequest, PrepareTransactionResponse, RegisterChunkServerRequest,
    RegisterChunkServerResponse, RenameRequest, RenameResponse,
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

        // Spawn transaction cleanup task
        let state_clone_tx = state.clone();
        let raft_tx_clone = raft_tx.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(5)); // Check every 5s
            loop {
                interval.tick().await;

                // Collect transactions that need cleanup
                let (timed_out_txs, stale_txs) = {
                    let state_lock = state_clone_tx.lock().unwrap();
                    if let AppState::Master(ref state) = *state_lock {
                        let timed_out: Vec<String> = state
                            .transaction_records
                            .iter()
                            .filter(|(_, record)| {
                                (record.state == TxState::Pending
                                    || record.state == TxState::Prepared)
                                    && record.is_timed_out()
                            })
                            .map(|(tx_id, _)| tx_id.clone())
                            .collect();

                        let stale: Vec<String> = state
                            .transaction_records
                            .iter()
                            .filter(|(_, record)| {
                                (record.state == TxState::Committed
                                    || record.state == TxState::Aborted)
                                    && record.is_stale()
                            })
                            .map(|(tx_id, _)| tx_id.clone())
                            .collect();

                        (timed_out, stale)
                    } else {
                        (vec![], vec![])
                    }
                };

                // Abort timed-out transactions
                for tx_id in timed_out_txs {
                    println!("Transaction {} timed out, aborting...", tx_id);
                    let (tx, rx) = tokio::sync::oneshot::channel();
                    let _ = raft_tx_clone
                        .send(Event::ClientRequest {
                            command: Command::Master(MasterCommand::UpdateTransactionState {
                                tx_id: tx_id.clone(),
                                new_state: TxState::Aborted,
                            }),
                            reply_tx: tx,
                        })
                        .await;
                    let _ = rx.await;
                }

                // Garbage collect stale transactions
                for tx_id in stale_txs {
                    println!("Transaction {} is stale, removing...", tx_id);
                    let (tx, rx) = tokio::sync::oneshot::channel();
                    let _ = raft_tx_clone
                        .send(Event::ClientRequest {
                            command: Command::Master(MasterCommand::DeleteTransactionRecord {
                                tx_id: tx_id.clone(),
                            }),
                            reply_tx: tx,
                        })
                        .await;
                    let _ = rx.await;
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
        self.check_shard_ownership(&req.path)?;

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

        self.check_shard_ownership(&req.path)?;

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
        request: Request<CompleteFileRequest>,
    ) -> Result<Response<CompleteFileResponse>, Status> {
        let req = request.into_inner();
        self.check_shard_ownership(&req.path)?;
        // No-op for now, but good to have the RPC
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

    // =========================================================================
    // Rename RPC Handler (Coordinator for Cross-Shard Operations)
    // =========================================================================
    async fn rename(
        &self,
        request: Request<RenameRequest>,
    ) -> Result<Response<RenameResponse>, Status> {
        let req = request.into_inner();
        let source_path = req.source_path;
        let dest_path = req.dest_path;

        // Check source shard ownership
        self.check_shard_ownership(&source_path)?;

        // Determine source and dest shard IDs
        let (source_shard, dest_shard, dest_peers) = {
            let map = self.shard_map.lock().unwrap();
            let source = map
                .get_shard(&source_path)
                .unwrap_or_else(|| self.shard_id.clone());
            let dest = map
                .get_shard(&dest_path)
                .unwrap_or_else(|| self.shard_id.clone());
            let peers = map.get_shard_peers(&dest).unwrap_or_default();
            (source, dest, peers)
        };

        // Check if source file exists and get its metadata
        let source_metadata = {
            let state_lock = self.state.lock().unwrap();
            if let AppState::Master(ref state) = *state_lock {
                match state.files.get(&source_path) {
                    Some(meta) => meta.clone(),
                    None => {
                        return Ok(Response::new(RenameResponse {
                            success: false,
                            error_message: format!("Source file not found: {}", source_path),
                            leader_hint: "".to_string(),
                            redirect_hint: "".to_string(),
                        }));
                    }
                }
            } else {
                return Err(Status::internal("Wrong state type"));
            }
        };

        // Same-shard rename: simple atomic operation
        if source_shard == dest_shard {
            let (tx, rx) = tokio::sync::oneshot::channel();
            if self
                .raft_tx
                .send(Event::ClientRequest {
                    command: Command::Master(MasterCommand::RenameFile {
                        source_path: source_path.clone(),
                        dest_path: dest_path.clone(),
                    }),
                    reply_tx: tx,
                })
                .await
                .is_err()
            {
                return Err(Status::internal("Raft channel closed"));
            }

            match rx.await {
                Ok(Ok(())) => Ok(Response::new(RenameResponse {
                    success: true,
                    error_message: "".to_string(),
                    leader_hint: "".to_string(),
                    redirect_hint: "".to_string(),
                })),
                Ok(Err(leader_opt)) => Ok(Response::new(RenameResponse {
                    success: false,
                    error_message: "Not Leader".to_string(),
                    leader_hint: leader_opt.unwrap_or_default(),
                    redirect_hint: "".to_string(),
                })),
                Err(_) => Err(Status::internal("Raft response error")),
            }
        } else {
            // Cross-shard rename: use Transaction Record pattern
            let tx_id = Uuid::new_v4().to_string();

            // Create dest metadata with new path
            let mut dest_metadata = source_metadata.clone();
            dest_metadata.path = dest_path.clone();

            // Step 1: Create Transaction Record (state = Pending)
            let tx_record = TransactionRecord::new_rename(
                tx_id.clone(),
                source_path.clone(),
                dest_path.clone(),
                source_shard.clone(),
                dest_shard.clone(),
                dest_metadata.clone(),
            );

            let (tx, rx) = tokio::sync::oneshot::channel();
            if self
                .raft_tx
                .send(Event::ClientRequest {
                    command: Command::Master(MasterCommand::CreateTransactionRecord {
                        record: tx_record,
                    }),
                    reply_tx: tx,
                })
                .await
                .is_err()
            {
                return Err(Status::internal("Raft channel closed"));
            }

            if let Err(leader_opt) = rx
                .await
                .map_err(|_| Status::internal("Raft response error"))?
            {
                return Ok(Response::new(RenameResponse {
                    success: false,
                    error_message: "Not Leader".to_string(),
                    leader_hint: leader_opt.unwrap_or_default(),
                    redirect_hint: "".to_string(),
                }));
            }

            // Step 2: Send PrepareTransaction to dest shard
            let prepare_result = self
                .send_prepare_to_dest_shard(&tx_id, &dest_path, &dest_metadata, &dest_peers)
                .await;

            match prepare_result {
                Ok(true) => {
                    // Step 3: Update to Prepared state
                    let (tx, rx) = tokio::sync::oneshot::channel();
                    let _ = self
                        .raft_tx
                        .send(Event::ClientRequest {
                            command: Command::Master(MasterCommand::UpdateTransactionState {
                                tx_id: tx_id.clone(),
                                new_state: TxState::Prepared,
                            }),
                            reply_tx: tx,
                        })
                        .await;
                    let _ = rx.await;

                    // Step 4: Delete source file and update to Committed
                    let (tx, rx) = tokio::sync::oneshot::channel();
                    let _ = self
                        .raft_tx
                        .send(Event::ClientRequest {
                            command: Command::Master(MasterCommand::ApplyTransactionOperation {
                                tx_id: tx_id.clone(),
                                operation: TxOperation {
                                    shard_id: source_shard.clone(),
                                    op_type: TxOpType::Delete {
                                        path: source_path.clone(),
                                    },
                                },
                            }),
                            reply_tx: tx,
                        })
                        .await;
                    let _ = rx.await;

                    let (tx, rx) = tokio::sync::oneshot::channel();
                    let _ = self
                        .raft_tx
                        .send(Event::ClientRequest {
                            command: Command::Master(MasterCommand::UpdateTransactionState {
                                tx_id: tx_id.clone(),
                                new_state: TxState::Committed,
                            }),
                            reply_tx: tx,
                        })
                        .await;
                    let _ = rx.await;

                    // Step 5: Send CommitTransaction to dest shard
                    let _ = self.send_commit_to_dest_shard(&tx_id, &dest_peers).await;

                    Ok(Response::new(RenameResponse {
                        success: true,
                        error_message: "".to_string(),
                        leader_hint: "".to_string(),
                        redirect_hint: "".to_string(),
                    }))
                }
                Ok(false) | Err(_) => {
                    // Prepare failed - abort transaction
                    let (tx, rx) = tokio::sync::oneshot::channel();
                    let _ = self
                        .raft_tx
                        .send(Event::ClientRequest {
                            command: Command::Master(MasterCommand::UpdateTransactionState {
                                tx_id: tx_id.clone(),
                                new_state: TxState::Aborted,
                            }),
                            reply_tx: tx,
                        })
                        .await;
                    let _ = rx.await;

                    Ok(Response::new(RenameResponse {
                        success: false,
                        error_message: "Cross-shard prepare failed".to_string(),
                        leader_hint: "".to_string(),
                        redirect_hint: "".to_string(),
                    }))
                }
            }
        }
    }

    // =========================================================================
    // PrepareTransaction RPC Handler (Participant in Cross-Shard Operations)
    // =========================================================================
    async fn prepare_transaction(
        &self,
        request: Request<PrepareTransactionRequest>,
    ) -> Result<Response<PrepareTransactionResponse>, Status> {
        let req = request.into_inner();
        let tx_id = req.tx_id;
        let path = req.path;
        let metadata = req.metadata;

        // Check shard ownership
        self.check_shard_ownership(&path)?;

        // Validate: dest file should not exist
        {
            let state_lock = self.state.lock().unwrap();
            if let AppState::Master(ref state) = *state_lock {
                if state.files.contains_key(&path) {
                    return Ok(Response::new(PrepareTransactionResponse {
                        success: false,
                        error_message: format!("Destination file already exists: {}", path),
                        leader_hint: "".to_string(),
                    }));
                }
            }
        }

        // Create Transaction Record (state = Prepared) with CREATE operation
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;

        let tx_record = TransactionRecord {
            tx_id: tx_id.clone(),
            tx_type: TransactionType::Rename {
                source_path: "".to_string(), // Unknown at participant
                dest_path: path.clone(),
            },
            state: TxState::Prepared,
            timestamp: now,
            participants: vec![req.coordinator_shard.clone(), self.shard_id.clone()],
            operations: vec![TxOperation {
                shard_id: self.shard_id.clone(),
                op_type: TxOpType::Create {
                    path: path.clone(),
                    metadata: metadata.unwrap_or_default(),
                },
            }],
        };

        let (tx, rx) = tokio::sync::oneshot::channel();
        if self
            .raft_tx
            .send(Event::ClientRequest {
                command: Command::Master(MasterCommand::CreateTransactionRecord {
                    record: tx_record,
                }),
                reply_tx: tx,
            })
            .await
            .is_err()
        {
            return Err(Status::internal("Raft channel closed"));
        }

        match rx.await {
            Ok(Ok(())) => Ok(Response::new(PrepareTransactionResponse {
                success: true,
                error_message: "".to_string(),
                leader_hint: "".to_string(),
            })),
            Ok(Err(leader_opt)) => Ok(Response::new(PrepareTransactionResponse {
                success: false,
                error_message: "Not Leader".to_string(),
                leader_hint: leader_opt.unwrap_or_default(),
            })),
            Err(_) => Err(Status::internal("Raft response error")),
        }
    }

    // =========================================================================
    // CommitTransaction RPC Handler
    // =========================================================================
    async fn commit_transaction(
        &self,
        request: Request<CommitTransactionRequest>,
    ) -> Result<Response<CommitTransactionResponse>, Status> {
        let req = request.into_inner();
        let tx_id = req.tx_id;

        // Find transaction record and apply the operation
        let operation = {
            let state_lock = self.state.lock().unwrap();
            if let AppState::Master(ref state) = *state_lock {
                state
                    .transaction_records
                    .get(&tx_id)
                    .and_then(|record| record.operations.first().cloned())
            } else {
                None
            }
        };

        if let Some(op) = operation {
            // Apply the operation
            let (tx, rx) = tokio::sync::oneshot::channel();
            if self
                .raft_tx
                .send(Event::ClientRequest {
                    command: Command::Master(MasterCommand::ApplyTransactionOperation {
                        tx_id: tx_id.clone(),
                        operation: op,
                    }),
                    reply_tx: tx,
                })
                .await
                .is_err()
            {
                return Err(Status::internal("Raft channel closed"));
            }

            if let Err(leader_opt) = rx
                .await
                .map_err(|_| Status::internal("Raft response error"))?
            {
                return Ok(Response::new(CommitTransactionResponse {
                    success: false,
                    error_message: "Not Leader".to_string(),
                    leader_hint: leader_opt.unwrap_or_default(),
                }));
            }

            // Update state to Committed
            let (tx, rx) = tokio::sync::oneshot::channel();
            let _ = self
                .raft_tx
                .send(Event::ClientRequest {
                    command: Command::Master(MasterCommand::UpdateTransactionState {
                        tx_id: tx_id.clone(),
                        new_state: TxState::Committed,
                    }),
                    reply_tx: tx,
                })
                .await;
            let _ = rx.await;

            Ok(Response::new(CommitTransactionResponse {
                success: true,
                error_message: "".to_string(),
                leader_hint: "".to_string(),
            }))
        } else {
            Ok(Response::new(CommitTransactionResponse {
                success: false,
                error_message: format!("Transaction not found: {}", tx_id),
                leader_hint: "".to_string(),
            }))
        }
    }

    // =========================================================================
    // AbortTransaction RPC Handler
    // =========================================================================
    async fn abort_transaction(
        &self,
        request: Request<AbortTransactionRequest>,
    ) -> Result<Response<AbortTransactionResponse>, Status> {
        let req = request.into_inner();
        let tx_id = req.tx_id;

        // Update state to Aborted
        let (tx, rx) = tokio::sync::oneshot::channel();
        if self
            .raft_tx
            .send(Event::ClientRequest {
                command: Command::Master(MasterCommand::UpdateTransactionState {
                    tx_id: tx_id.clone(),
                    new_state: TxState::Aborted,
                }),
                reply_tx: tx,
            })
            .await
            .is_err()
        {
            return Err(Status::internal("Raft channel closed"));
        }

        match rx.await {
            Ok(Ok(())) => Ok(Response::new(AbortTransactionResponse {
                success: true,
                error_message: "".to_string(),
                leader_hint: "".to_string(),
            })),
            Ok(Err(leader_opt)) => Ok(Response::new(AbortTransactionResponse {
                success: false,
                error_message: "Not Leader".to_string(),
                leader_hint: leader_opt.unwrap_or_default(),
            })),
            Err(_) => Err(Status::internal("Raft response error")),
        }
    }
}

// ============================================================================
// Helper methods for cross-shard communication
// ============================================================================
impl MyMaster {
    /// Send PrepareTransaction RPC to destination shard
    async fn send_prepare_to_dest_shard(
        &self,
        tx_id: &str,
        path: &str,
        metadata: &FileMetadata,
        dest_peers: &[String],
    ) -> Result<bool, Status> {
        use crate::dfs::master_service_client::MasterServiceClient;

        for peer in dest_peers {
            let addr = if peer.starts_with("http://") {
                peer.clone()
            } else {
                format!("http://{}", peer)
            };

            match MasterServiceClient::connect(addr.clone()).await {
                Ok(mut client) => {
                    let request = tonic::Request::new(PrepareTransactionRequest {
                        tx_id: tx_id.to_string(),
                        operation_type: "CREATE".to_string(),
                        path: path.to_string(),
                        metadata: Some(metadata.clone()),
                        coordinator_shard: self.shard_id.clone(),
                        coordinator_peers: vec![], // Could add self peers here
                    });

                    match client.prepare_transaction(request).await {
                        Ok(response) => {
                            let resp = response.into_inner();
                            if resp.success {
                                return Ok(true);
                            }
                            if !resp.leader_hint.is_empty() {
                                // Retry with leader hint
                                continue;
                            }
                            return Ok(false);
                        }
                        Err(e) => {
                            eprintln!("PrepareTransaction failed to {}: {}", addr, e);
                            continue;
                        }
                    }
                }
                Err(e) => {
                    eprintln!("Failed to connect to {}: {}", addr, e);
                    continue;
                }
            }
        }

        Ok(false)
    }

    /// Send CommitTransaction RPC to destination shard
    async fn send_commit_to_dest_shard(
        &self,
        tx_id: &str,
        dest_peers: &[String],
    ) -> Result<bool, Status> {
        use crate::dfs::master_service_client::MasterServiceClient;

        for peer in dest_peers {
            let addr = if peer.starts_with("http://") {
                peer.clone()
            } else {
                format!("http://{}", peer)
            };

            match MasterServiceClient::connect(addr.clone()).await {
                Ok(mut client) => {
                    let request = tonic::Request::new(CommitTransactionRequest {
                        tx_id: tx_id.to_string(),
                    });

                    match client.commit_transaction(request).await {
                        Ok(response) => {
                            let resp = response.into_inner();
                            if resp.success {
                                return Ok(true);
                            }
                            if !resp.leader_hint.is_empty() {
                                continue;
                            }
                            return Ok(false);
                        }
                        Err(e) => {
                            eprintln!("CommitTransaction failed to {}: {}", addr, e);
                            continue;
                        }
                    }
                }
                Err(e) => {
                    eprintln!("Failed to connect to {}: {}", addr, e);
                    continue;
                }
            }
        }

        Ok(false)
    }
}

// ============================================================================
// Unit Tests
// ============================================================================
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tx_state_equality() {
        assert_eq!(TxState::Pending, TxState::Pending);
        assert_eq!(TxState::Prepared, TxState::Prepared);
        assert_eq!(TxState::Committed, TxState::Committed);
        assert_eq!(TxState::Aborted, TxState::Aborted);
        assert_ne!(TxState::Pending, TxState::Committed);
    }

    #[test]
    fn test_transaction_record_new_rename() {
        let metadata = FileMetadata {
            path: "/source/file.txt".to_string(),
            size: 1024,
            blocks: vec![],
        };

        let tx_record = TransactionRecord::new_rename(
            "test-tx-id".to_string(),
            "/source/file.txt".to_string(),
            "/dest/file.txt".to_string(),
            "shard-1".to_string(),
            "shard-2".to_string(),
            metadata,
        );

        assert_eq!(tx_record.tx_id, "test-tx-id");
        assert_eq!(tx_record.state, TxState::Pending);
        assert_eq!(tx_record.participants.len(), 2);
        assert_eq!(tx_record.participants[0], "shard-1");
        assert_eq!(tx_record.participants[1], "shard-2");
        assert_eq!(tx_record.operations.len(), 2);

        // First operation should be DELETE
        match &tx_record.operations[0].op_type {
            TxOpType::Delete { path } => assert_eq!(path, "/source/file.txt"),
            _ => panic!("Expected Delete operation"),
        }

        // Second operation should be CREATE
        match &tx_record.operations[1].op_type {
            TxOpType::Create { path, metadata: _ } => assert_eq!(path, "/dest/file.txt"),
            _ => panic!("Expected Create operation"),
        }
    }

    #[test]
    fn test_transaction_record_deletes_file() {
        let metadata = FileMetadata {
            path: "/source/file.txt".to_string(),
            size: 1024,
            blocks: vec![],
        };

        let tx_record = TransactionRecord::new_rename(
            "test-tx-id".to_string(),
            "/source/file.txt".to_string(),
            "/dest/file.txt".to_string(),
            "shard-1".to_string(),
            "shard-2".to_string(),
            metadata,
        );

        assert!(tx_record.deletes_file("/source/file.txt"));
        assert!(!tx_record.deletes_file("/other/file.txt"));
    }

    #[test]
    fn test_transaction_record_creates_file() {
        let metadata = FileMetadata {
            path: "/source/file.txt".to_string(),
            size: 1024,
            blocks: vec![],
        };

        let tx_record = TransactionRecord::new_rename(
            "test-tx-id".to_string(),
            "/source/file.txt".to_string(),
            "/dest/file.txt".to_string(),
            "shard-1".to_string(),
            "shard-2".to_string(),
            metadata,
        );

        assert!(tx_record.creates_file("/dest/file.txt"));
        assert!(!tx_record.creates_file("/other/file.txt"));
    }

    #[test]
    fn test_transaction_record_get_created_metadata() {
        let metadata = FileMetadata {
            path: "/dest/file.txt".to_string(),
            size: 1024,
            blocks: vec![BlockInfo {
                block_id: "block-1".to_string(),
                size: 1024,
                locations: vec!["chunk1:50052".to_string()],
            }],
        };

        let tx_record = TransactionRecord::new_rename(
            "test-tx-id".to_string(),
            "/source/file.txt".to_string(),
            "/dest/file.txt".to_string(),
            "shard-1".to_string(),
            "shard-2".to_string(),
            metadata.clone(),
        );

        let created_meta = tx_record.get_created_metadata("/dest/file.txt");
        assert!(created_meta.is_some());
        let created_meta = created_meta.unwrap();
        assert_eq!(created_meta.size, 1024);
        assert_eq!(created_meta.blocks.len(), 1);

        let not_found = tx_record.get_created_metadata("/other/file.txt");
        assert!(not_found.is_none());
    }

    #[test]
    fn test_transaction_type_rename() {
        let tx_type = TransactionType::Rename {
            source_path: "/source/file.txt".to_string(),
            dest_path: "/dest/file.txt".to_string(),
        };

        match tx_type {
            TransactionType::Rename {
                source_path,
                dest_path,
            } => {
                assert_eq!(source_path, "/source/file.txt");
                assert_eq!(dest_path, "/dest/file.txt");
            }
        }
    }

    #[test]
    fn test_master_state_default() {
        let state = MasterState::default();
        assert!(state.files.is_empty());
        assert!(state.transaction_records.is_empty());
        assert!(state.chunk_servers.is_empty());
        assert!(state.pending_commands.is_empty());
    }

    #[test]
    fn test_master_state_with_transaction_records() {
        let mut state = MasterState::default();

        let metadata = FileMetadata {
            path: "/test.txt".to_string(),
            size: 100,
            blocks: vec![],
        };

        let tx_record = TransactionRecord::new_rename(
            "tx-1".to_string(),
            "/source.txt".to_string(),
            "/dest.txt".to_string(),
            "shard-1".to_string(),
            "shard-2".to_string(),
            metadata,
        );

        state
            .transaction_records
            .insert("tx-1".to_string(), tx_record);

        assert_eq!(state.transaction_records.len(), 1);
        assert!(state.transaction_records.contains_key("tx-1"));

        let record = state.transaction_records.get("tx-1").unwrap();
        assert_eq!(record.state, TxState::Pending);
    }
}
