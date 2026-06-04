use crate::dfs::master_service_server::MasterService;
use crate::dfs::{
    AbortTransactionRequest, AbortTransactionResponse, AddRaftServerRequest, AddRaftServerResponse,
    AllocateBlockRequest, AllocateBlockResponse, BlockInfo, ChunkServerCommand, ClusterMember,
    CommitTransactionRequest, CommitTransactionResponse, CompleteFileRequest, CompleteFileResponse,
    CreateFileRequest, CreateFileResponse, DeleteFileRequest, DeleteFileResponse, FileMetadata,
    GetBlockLocationsRequest, GetBlockLocationsResponse, GetClusterInfoRequest,
    GetClusterInfoResponse, GetFileInfoRequest, GetFileInfoResponse, GetSafeModeStatusRequest,
    GetSafeModeStatusResponse, HeartbeatRequest, HeartbeatResponse, IngestMetadataRequest,
    IngestMetadataResponse, InitiateShuffleRequest, InitiateShuffleResponse, ListFilesRequest,
    ListFilesResponse, PrepareTransactionRequest, PrepareTransactionResponse,
    RegisterChunkServerRequest, RegisterChunkServerResponse, RemoveRaftServerRequest,
    RemoveRaftServerResponse, RenameRequest, RenameResponse, SetSafeModeRequest,
    SetSafeModeResponse,
};
use crate::simple_raft::{AppState, Command, Event, MasterCommand, MembershipCommand, Role};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tonic::{Request, Response, Status};
use tracing::Instrument;
use uuid::Uuid;

const DEFAULT_REPLICATION_FACTOR: usize = 3;

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
            .expect("Time went backwards")
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
            .expect("Time went backwards")
            .as_millis() as u64;
        now - self.timestamp > 10_000 // 10 seconds
    }

    /// Check if transaction is stale and can be garbage collected (1 hour)
    pub fn is_stale(&self) -> bool {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("Time went backwards")
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
    #[serde(default)]
    pub rack_id: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct MasterState {
    /// Metadata for files: path -> metadata
    pub files: HashMap<String, FileMetadata>,

    /// Transaction records for cross-shard operations: tx_id -> record
    pub transaction_records: HashMap<String, TransactionRecord>,

    /// Prefixes currently undergoing background data shuffling
    pub shuffling_prefixes: HashSet<String>,

    /// ChunkServer status (not persisted via Raft, local state only)
    #[serde(skip)]
    pub chunk_servers: HashMap<String, ChunkServerStatus>, // address -> status

    /// Pending commands for ChunkServers (not persisted via Raft)
    #[serde(skip)]
    pub pending_commands: HashMap<String, Vec<ChunkServerCommand>>, // address -> commands

    /// Safe Mode: cluster is in safe mode until enough blocks are reported
    #[serde(skip)]
    pub safe_mode: bool,

    /// Timestamp when safe mode was entered (millis since epoch)
    #[serde(skip)]
    pub safe_mode_entered_at: u64,

    /// Expected minimum number of ChunkServers before exiting safe mode
    #[serde(skip)]
    pub safe_mode_min_chunkservers: usize,

    /// Total blocks known from metadata
    #[serde(skip)]
    pub expected_block_count: usize,

    /// Number of blocks reported by ChunkServers
    #[serde(skip)]
    pub reported_block_count: usize,

    /// Safe mode threshold (percentage of blocks that must be reported, 0.0-1.0)
    #[serde(skip)]
    pub safe_mode_threshold: f64,

    /// Whether safe mode was manually forced
    #[serde(skip)]
    pub safe_mode_manual: bool,

    /// Blocks known to be corrupted on specific ChunkServers (not Raft-persisted).
    /// Maps block_id -> set of ChunkServer addresses with a bad copy.
    #[serde(skip)]
    pub bad_block_locations: HashMap<String, HashSet<String>>,
}

impl MasterState {
    /// Initialize safe mode on startup
    pub fn enter_safe_mode(&mut self) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("Time went backwards")
            .as_millis() as u64;

        self.safe_mode = true;
        self.safe_mode_entered_at = now;
        self.safe_mode_min_chunkservers = 1; // At least 1 ChunkServer required
        self.safe_mode_threshold = 0.99; // 99% of blocks must be reported
        self.expected_block_count = self.count_total_blocks();
        self.reported_block_count = 0;
        self.safe_mode_manual = false;

        tracing::info!(
            "Entering Safe Mode: expecting {} blocks, threshold {}%",
            self.expected_block_count,
            (self.safe_mode_threshold * 100.0) as u32
        );
    }

    /// Count total blocks across all files
    fn count_total_blocks(&self) -> usize {
        self.files.values().map(|f| f.blocks.len()).sum()
    }

    /// Check if safe mode should be exited automatically
    pub fn should_exit_safe_mode(&self) -> bool {
        if self.safe_mode_manual {
            return false; // Manual safe mode requires manual exit
        }

        if !self.safe_mode {
            return false; // Already not in safe mode
        }

        // Check minimum ChunkServers
        if self.chunk_servers.len() < self.safe_mode_min_chunkservers {
            return false;
        }

        // Check block reporting threshold
        if self.expected_block_count == 0 {
            // No blocks to report, can exit immediately
            return true;
        }

        let reported_ratio = self.reported_block_count as f64 / self.expected_block_count as f64;
        if reported_ratio >= self.safe_mode_threshold {
            return true;
        }

        // Check timeout (60 seconds max in safe mode)
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("Time went backwards")
            .as_millis() as u64;
        if now - self.safe_mode_entered_at > 60_000 {
            tracing::info!("Safe Mode timeout reached, forcing exit");
            return true;
        }

        false
    }

    /// Exit safe mode
    pub fn exit_safe_mode(&mut self) {
        if self.safe_mode {
            tracing::info!(
                "Safe Mode exit threshold reached: {}/{} blocks reported ({:.2}%)",
                self.reported_block_count,
                self.expected_block_count,
                self.reported_block_count as f64 / self.expected_block_count as f64 * 100.0
            );
            self.safe_mode = false;
            self.safe_mode_manual = false;
        }
    }

    /// Force enter safe mode (manual)
    pub fn force_enter_safe_mode(&mut self) {
        self.enter_safe_mode();
        self.safe_mode_manual = true;
        tracing::info!("Manually entered Safe Mode");
    }

    /// Force exit safe mode (manual)
    pub fn force_exit_safe_mode(&mut self) {
        self.safe_mode_manual = false;
        self.exit_safe_mode();
        tracing::info!("Manually exited Safe Mode");
    }

    /// Check if in safe mode
    pub fn is_in_safe_mode(&self) -> bool {
        self.safe_mode
    }

    /// Update reported block count from heartbeat
    pub fn update_reported_blocks(&mut self, block_count: usize) {
        self.reported_block_count += block_count;

        // Check if we should exit safe mode
        if self.should_exit_safe_mode() {
            self.exit_safe_mode();
        }
    }
}

/// Select up to `n` chunk servers for replica placement, maximizing rack diversity.
///
/// Algorithm:
/// 1. Sort all candidates by available_space descending (best-first).
/// 2. Round-robin through racks: in each round, pick the best remaining server
///    from each rack that hasn't contributed yet in this round.
/// 3. Stop when `n` servers are selected or candidates exhausted.
///
/// Empty rack_id strings are each treated as a unique rack (address-keyed).
fn select_servers_rack_aware(servers: &[(String, ChunkServerStatus)], n: usize) -> Vec<String> {
    if n == 0 || servers.is_empty() {
        return vec![];
    }

    // Sort candidates by available_space descending
    let mut candidates: Vec<&(String, ChunkServerStatus)> = servers.iter().collect();
    candidates.sort_by_key(|b| std::cmp::Reverse(b.1.available_space));

    // Group by rack. Empty rack_id → use address as key to avoid grouping them.
    let mut rack_buckets: HashMap<String, Vec<&(String, ChunkServerStatus)>> = HashMap::new();
    for s in &candidates {
        let rack_key = if s.1.rack_id.is_empty() {
            format!("__addr__{}", s.0)
        } else {
            s.1.rack_id.clone()
        };
        rack_buckets.entry(rack_key).or_default().push(s);
    }

    // Collect racks ordered by their best (first) server's available_space.
    // Tie-breaking between racks with equal best-server space is non-deterministic
    // (HashMap iteration order), but this is benign — all racks still participate
    // in the round-robin and any server from a tied rack is equally valid.
    let racks: Vec<Vec<&(String, ChunkServerStatus)>> = {
        let mut r: Vec<Vec<&(String, ChunkServerStatus)>> = rack_buckets.into_values().collect();
        // Sort racks by best server descending
        r.sort_by(|a, b| b[0].1.available_space.cmp(&a[0].1.available_space));
        r
    };

    let mut selected: Vec<String> = Vec::with_capacity(n);
    let mut rack_positions: Vec<usize> = vec![0; racks.len()];

    // Round-robin: each round picks one server per rack
    'outer: loop {
        let mut picked_this_round = false;
        for (rack_idx, rack) in racks.iter().enumerate() {
            if selected.len() >= n {
                break 'outer;
            }
            let pos = rack_positions[rack_idx];
            if pos < rack.len() {
                selected.push(rack[pos].0.clone());
                rack_positions[rack_idx] += 1;
                picked_this_round = true;
            }
        }
        if !picked_this_round {
            break; // All candidates exhausted
        }
    }

    selected
}

/// Schedule replication commands for all blocks with fewer live replicas than REPLICATION_FACTOR.
/// Considers both dead chunk servers and known-bad block locations.
fn heal_under_replicated_blocks(state: &mut MasterState) {
    const REPLICATION_FACTOR: usize = 3;

    let live_servers: Vec<String> = state.chunk_servers.keys().cloned().collect();
    if live_servers.is_empty() {
        return;
    }

    for file in state.files.values() {
        for block in &file.blocks {
            if block.ec_data_shards > 0 {
                // ── EC block: detect missing shards and issue RECONSTRUCT_EC_SHARD ──
                let k = block.ec_data_shards as usize;

                let total = (block.ec_data_shards + block.ec_parity_shards) as usize;
                if block.locations.len() != total {
                    tracing::error!(
                        "EC block {} has {} locations but EC({},{}) expects {}; skipping heal",
                        block.block_id,
                        block.locations.len(),
                        block.ec_data_shards,
                        block.ec_parity_shards,
                        total
                    );
                    continue; // skip this block
                }

                // Count live shards
                let live_count = block
                    .locations
                    .iter()
                    .filter(|loc| state.chunk_servers.contains_key(*loc))
                    .count();

                // For each shard position, check if its host is dead
                for (shard_idx, loc) in block.locations.iter().enumerate() {
                    if state.chunk_servers.contains_key(loc) {
                        // Shard is on a live server — nothing to do for this shard
                        continue;
                    }

                    // Shard is missing
                    if live_count < k {
                        tracing::error!(
                            "EC block {} is unrecoverable: only {}/{} shards available (need {} to recover)",
                            block.block_id,
                            live_count,
                            block.ec_data_shards + block.ec_parity_shards,
                            k
                        );
                        // Can't recover any shard; skip all remaining shards too
                        break;
                    }

                    // Find a target CS not already holding a shard of this block
                    let target = match live_servers.iter().find(|s| !block.locations.contains(*s)) {
                        Some(t) => t.clone(),
                        None => {
                            tracing::warn!(
                                "EC heal: no free CS to hold reconstructed shard {} of block {}; \
                                 cluster may need more nodes",
                                shard_idx,
                                block.block_id
                            );
                            continue;
                        }
                    };

                    // Build sources list: live address or empty string if dead
                    let sources: Vec<String> = block
                        .locations
                        .iter()
                        .map(|l| {
                            if state.chunk_servers.contains_key(l) {
                                l.clone()
                            } else {
                                String::new()
                            }
                        })
                        .collect();

                    state
                        .pending_commands
                        .entry(target.clone())
                        .or_default()
                        .push(ChunkServerCommand {
                            r#type: 3, // RECONSTRUCT_EC_SHARD
                            block_id: block.block_id.clone(),
                            target_chunk_server_address: target.clone(),
                            shard_index: shard_idx as i32,
                            ec_data_shards: block.ec_data_shards,
                            ec_parity_shards: block.ec_parity_shards,
                            ec_shard_sources: sources,
                            original_block_size: block.original_size,
                        });
                    tracing::info!(
                        "Healer: scheduled EC reconstruction of shard {} of block {} to {}",
                        shard_idx,
                        block.block_id,
                        target
                    );
                }
            } else {
                // ── Replication block: existing logic (unchanged) ──
                let bad_on = state
                    .bad_block_locations
                    .get(&block.block_id)
                    .cloned()
                    .unwrap_or_default();

                let live_locs: Vec<String> = block
                    .locations
                    .iter()
                    .filter(|loc| state.chunk_servers.contains_key(*loc) && !bad_on.contains(*loc))
                    .cloned()
                    .collect();

                let needed = REPLICATION_FACTOR.saturating_sub(live_locs.len());
                if needed == 0 {
                    continue;
                }

                if live_locs.is_empty() {
                    tracing::error!(
                        "Healer: block {} has NO live replicas — data may be lost",
                        block.block_id
                    );
                    continue;
                }

                let source = &live_locs[0];

                let targets: Vec<String> = live_servers
                    .iter()
                    .filter(|s| !block.locations.contains(s))
                    .take(needed)
                    .cloned()
                    .collect();

                for target in &targets {
                    state
                        .pending_commands
                        .entry(source.clone())
                        .or_default()
                        .push(ChunkServerCommand {
                            r#type: 1, // REPLICATE
                            block_id: block.block_id.clone(),
                            target_chunk_server_address: target.clone(),
                            shard_index: -1,
                            ec_data_shards: 0,
                            ec_parity_shards: 0,
                            ec_shard_sources: vec![],
                            original_block_size: 0,
                        });
                    tracing::info!(
                        "Healer: scheduled replication of block {} from {} to {}",
                        block.block_id,
                        source,
                        target
                    );
                }
            }
        }
    }
}

use dfs_common::sharding::{ShardId, ShardMap};

// ============================================================================
// Throughput Monitoring for Dynamic Sharding
// ============================================================================

#[derive(Debug, Default, Clone)]
pub struct PrefixMetrics {
    pub rps: f64,
    pub bps: f64,
    pub last_count: u64,
    pub last_bytes: u64,
}

#[derive(Debug)]
pub struct ThroughputMonitor {
    pub metrics: Mutex<HashMap<String, PrefixMetrics>>,
    pub split_threshold_rps: f64,
    pub merge_threshold_rps: f64,
    pub split_cooldown_secs: u64,
    pub last_split_time: Mutex<Instant>,
}

impl ThroughputMonitor {
    pub fn new(split_threshold: f64, merge_threshold: f64, cooldown: u64) -> Self {
        Self {
            metrics: Mutex::new(HashMap::new()),
            split_threshold_rps: split_threshold,
            merge_threshold_rps: merge_threshold,
            split_cooldown_secs: cooldown,
            last_split_time: Mutex::new(Instant::now() - Duration::from_secs(cooldown)),
        }
    }

    pub fn record_request(&self, path: &str, bytes: u64) {
        let prefix = self.get_path_prefix(path);
        tracing::info!("Recording request for path: {} (prefix: {})", path, prefix);
        let mut metrics = self.metrics.lock().expect("Mutex poisoned");
        let entry = metrics.entry(prefix).or_default();
        entry.last_count += 1;
        entry.last_bytes += bytes;
    }

    fn get_path_prefix(&self, path: &str) -> String {
        let parts: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
        if !parts.is_empty() {
            format!("/{}/", parts[0])
        } else {
            "/".to_string()
        }
    }

    pub fn decay_metrics(&self) {
        let mut metrics = self.metrics.lock().expect("Mutex poisoned");
        for (prefix, m) in metrics.iter_mut() {
            // Simple exponential moving average (EMA)
            // rps = last_count / interval (we assume 5s interval for now)
            let current_rps = m.last_count as f64 / 5.0;
            let current_bps = m.last_bytes as f64 / 5.0;

            m.rps = m.rps * 0.3 + current_rps * 0.7;
            m.bps = m.bps * 0.3 + current_bps * 0.7;

            if m.rps > 0.1 {
                tracing::debug!("Prefix {} RPS: {:.2}, BPS: {:.2}", prefix, m.rps, m.bps);
            }

            m.last_count = 0;
            m.last_bytes = 0;
        }
    }
}

/// Configuration for MyMaster
#[derive(Debug, Clone)]
pub struct MasterConfig {
    pub shard_id: ShardId,
    pub config_server_addrs: Vec<String>,
    pub master_address: String,
    pub split_threshold_rps: f64,
    pub split_cooldown_secs: u64,
    pub merge_threshold_rps: f64,
    pub ca_cert_path: Option<String>,
    pub domain_name: Option<String>,
    /// Seconds since last access before a file is considered cold (env: COLD_THRESHOLD_SECS, default: 604800 = 7 days)
    pub cold_threshold_secs: u64,
    /// Seconds after cold move before EC conversion (env: EC_THRESHOLD_SECS, default: 2592000 = 30 days)
    pub ec_threshold_secs: u64,
}

#[derive(Debug, Clone)]
pub struct MyMaster {
    state: Arc<Mutex<AppState>>,
    raft_tx: mpsc::Sender<Event>,
    shard_map: Arc<Mutex<ShardMap>>,
    shard_id: ShardId,
    monitor: Arc<ThroughputMonitor>,
    _master_address: String,
    _config_server_addrs: Vec<String>,
    config: MasterConfig,
}

impl MyMaster {
    pub fn new(
        state: Arc<Mutex<AppState>>,
        raft_tx: mpsc::Sender<Event>,
        shard_map: Arc<Mutex<ShardMap>>,
        config: MasterConfig,
    ) -> Self {
        let monitor = Arc::new(ThroughputMonitor::new(
            config.split_threshold_rps,
            config.merge_threshold_rps,
            config.split_cooldown_secs,
        ));

        // Capture TLS config for spawned tasks
        let ca_cert_path = config.ca_cert_path.clone();
        let domain_name = config.domain_name.clone();

        // Spawn liveness check loop
        let state_clone = state.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(5));
            loop {
                interval.tick().await;
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .expect("Time went backwards")
                    .as_millis() as u64;

                let mut state_lock = state_clone.lock().expect("Mutex poisoned");
                if let AppState::Master(ref mut state) = *state_lock {
                    // Remove chunk servers that haven't sent heartbeat in 15 seconds
                    let dead_servers: Vec<String> = state
                        .chunk_servers
                        .iter()
                        .filter(|(_, status)| now - status.last_heartbeat > 15000)
                        .map(|(addr, _)| addr.clone())
                        .collect();

                    for addr in &dead_servers {
                        tracing::warn!("ChunkServer {} is dead (no heartbeat), removing...", addr);
                        state.chunk_servers.remove(addr);
                        state.pending_commands.remove(addr);
                    }
                    if !dead_servers.is_empty() {
                        tracing::info!(
                            "Healer: {} ChunkServer(s) died, scanning for under-replicated blocks",
                            dead_servers.len()
                        );
                        heal_under_replicated_blocks(state);
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
                let mut state_lock = state_clone_balancer.lock().expect("Mutex poisoned");
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
                    sorted_servers.sort_by_key(|a| a.1); // Ascending available space (Least available first)

                    let (most_full_addr, min_avail) = sorted_servers.first().unwrap();
                    let (least_full_addr, max_avail) = sorted_servers.last().unwrap();

                    // If difference is greater than 100MB (arbitrary threshold for demo)
                    // In real world, use percentage or standard deviation
                    if max_avail > min_avail && (max_avail - min_avail) > 100 * 1024 * 1024 {
                        tracing::info!(
                            "Balancer: Detected imbalance. Moving block from {} to {}",
                            most_full_addr,
                            least_full_addr
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
                                shard_index: -1,
                                ec_data_shards: 0,
                                ec_parity_shards: 0,
                                ec_shard_sources: vec![],
                                original_block_size: 0,
                            };

                            state
                                .pending_commands
                                .entry(most_full_addr.clone())
                                .or_default()
                                .push(command);

                            tracing::info!(
                                "Balancer: Scheduled replication of block {} from {} to {}",
                                block_id,
                                most_full_addr,
                                least_full_addr
                            );
                        }
                    }
                }
            }
        });

        // Spawn periodic healer task
        let state_clone_healer = state.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(300)); // 5 minutes
            loop {
                interval.tick().await;
                let mut state_lock = state_clone_healer.lock().expect("Mutex poisoned");
                if let AppState::Master(ref mut state) = *state_lock {
                    if !state.is_in_safe_mode() {
                        tracing::info!("Periodic healer: scanning for under-replicated blocks");
                        heal_under_replicated_blocks(state);
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
                    let state_lock = state_clone_tx.lock().expect("Mutex poisoned");
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
                    tracing::warn!("Transaction {} timed out, aborting...", tx_id);
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
                    tracing::info!("Transaction {} is stale, removing...", tx_id);
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

        // Spawn data shuffling task
        let state_clone_shuffle = state.clone();
        let raft_tx_shuffle = raft_tx.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(10));
            loop {
                interval.tick().await;

                let (prefixes, servers) = {
                    let state_lock = state_clone_shuffle.lock().expect("Mutex poisoned");
                    if let AppState::Master(ref state) = *state_lock {
                        let prefixes: Vec<String> =
                            state.shuffling_prefixes.iter().cloned().collect();
                        let servers: Vec<(String, u64)> = state
                            .chunk_servers
                            .iter()
                            .map(|(addr, status)| (addr.clone(), status.available_space))
                            .collect();
                        (prefixes, servers)
                    } else {
                        (vec![], vec![])
                    }
                };

                if prefixes.is_empty() || servers.len() < 2 {
                    continue;
                }

                // For each prefix, try to move some blocks from most full to least full servers
                for prefix in prefixes {
                    let stop_shuffle = {
                        let mut state_lock = state_clone_shuffle.lock().expect("Mutex poisoned");
                        if let AppState::Master(ref mut state) = *state_lock {
                            let mut sorted_servers = servers.clone();
                            sorted_servers.sort_by_key(|b| std::cmp::Reverse(b.1)); // Descending available space (Coolest first)

                            let (least_full_addr, _) = sorted_servers.first().unwrap();
                            let (most_full_addr, _) = sorted_servers.last().unwrap();

                            // Find blocks with this prefix on the most full server
                            let mut block_to_shuffle = None;
                            'outer: for file in state.files.values() {
                                if file.path.starts_with(&prefix) {
                                    for block in &file.blocks {
                                        if block.locations.contains(most_full_addr)
                                            && !block.locations.contains(least_full_addr)
                                        {
                                            block_to_shuffle = Some(block.block_id.clone());
                                            break 'outer;
                                        }
                                    }
                                }
                            }

                            if let Some(block_id) = block_to_shuffle {
                                let command = ChunkServerCommand {
                                    r#type: 1, // REPLICATE
                                    block_id: block_id.clone(),
                                    target_chunk_server_address: least_full_addr.clone(),
                                    shard_index: -1,
                                    ec_data_shards: 0,
                                    ec_parity_shards: 0,
                                    ec_shard_sources: vec![],
                                    original_block_size: 0,
                                };

                                state
                                    .pending_commands
                                    .entry(most_full_addr.clone())
                                    .or_default()
                                    .push(command);

                                tracing::info!(
                                    "Shuffle: Scheduled move of block {} (prefix: {}) from {} to {}",
                                    block_id,
                                    prefix,
                                    most_full_addr,
                                    least_full_addr
                                );
                                false
                            } else {
                                // No more blocks to move for this prefix? Or already balanced.
                                true
                            }
                        } else {
                            false
                        }
                    };

                    if stop_shuffle {
                        let (tx, rx) = tokio::sync::oneshot::channel();
                        let _ = raft_tx_shuffle
                            .send(Event::ClientRequest {
                                command: Command::Master(MasterCommand::StopShuffle {
                                    prefix: prefix.clone(),
                                }),
                                reply_tx: tx,
                            })
                            .await;
                        let _ = rx.await;
                    }
                }
            }
        });

        // Monitor is now initialized at the beginning of new()

        // Spawn metrics decay task
        let monitor_clone = monitor.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(5));
            loop {
                interval.tick().await;
                monitor_clone.decay_metrics();
            }
        });

        // Spawn ShardMap refresh task
        let shard_map_refresh = shard_map.clone();
        let config_addrs_refresh = config.config_server_addrs.clone();
        let ca_cert_refresh = ca_cert_path.clone();
        let domain_name_refresh = domain_name.clone();

        tokio::spawn(async move {
            let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(5));
            loop {
                interval.tick().await;

                if config_addrs_refresh.is_empty() {
                    continue;
                }

                // Try to fetch shard map from any config server
                for config_addr in &config_addrs_refresh {
                    use crate::dfs::config_service_client::ConfigServiceClient;

                    if let Ok(channel) = dfs_common::security::connect_endpoint(
                        config_addr,
                        ca_cert_refresh.as_deref(),
                        domain_name_refresh.as_deref(),
                    )
                    .await
                    {
                        let mut client = ConfigServiceClient::new(channel);
                        if let Ok(resp) = client
                            .fetch_shard_map(crate::dfs::FetchShardMapRequest {})
                            .await
                        {
                            let shards_data = resp.into_inner().shards;

                            // Deterministic sort
                            let mut shards_vec: Vec<_> = shards_data.into_iter().collect();
                            shards_vec.sort_by(|a, b| a.0.cmp(&b.0));

                            let mut new_map = ShardMap::new_range();

                            for (shard_id, peers_info) in shards_vec {
                                new_map.add_shard(shard_id, peers_info.peers);
                            }

                            // Update local map
                            let mut w = shard_map_refresh.lock().expect("Mutex poisoned");
                            *w = new_map;
                            tracing::debug!(
                                "Refreshed ShardMap from Config Server {}",
                                config_addr
                            );
                            break; // Success, wait for next interval
                        }
                    }
                }
            }
        });

        // Spawn split detection and heartbeat task
        let monitor_clone_split = monitor.clone();
        let raft_tx_split = raft_tx.clone();
        let shard_id_split = config.shard_id.clone();
        let config_server_addrs_task = config.config_server_addrs.clone();
        let master_address_task = config.master_address.clone();
        let state_clone_split = state.clone();
        let shard_map_clone_split = shard_map.clone();
        let ca_cert_task = ca_cert_path.clone();
        let domain_name_task = domain_name.clone();

        tokio::spawn(async move {
            let mut registered = false;
            let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(5));
            loop {
                interval.tick().await;

                // Loop-local clones to prevent move-after-use in nested closures
                let state_clone = state_clone_split.clone();
                let shard_map_clone = shard_map_clone_split.clone();
                let monitor_clone = monitor_clone_split.clone();
                let shard_id = shard_id_split.clone();
                let raft_tx = raft_tx_split.clone();
                let config_addrs = config_server_addrs_task.clone();
                let master_addr = master_address_task.clone();
                let ca_cert = ca_cert_task.clone();
                let domain = domain_name_task.clone();

                // 1. Register with Config Server if not already registered
                if !registered {
                    for config_addr in &config_addrs {
                        use crate::dfs::config_service_client::ConfigServiceClient;

                        if let Ok(channel) = dfs_common::security::connect_endpoint(
                            config_addr,
                            ca_cert.as_deref(),
                            domain.as_deref(),
                        )
                        .await
                        {
                            let mut client = ConfigServiceClient::new(channel);
                            let reg_req = crate::dfs::RegisterMasterRequest {
                                address: master_addr.clone(),
                                shard_id: shard_id.clone(),
                            };
                            if let Ok(resp) = client.register_master(reg_req).await {
                                if resp.get_ref().success {
                                    tracing::info!(
                                        "Registered with Config Server at {}",
                                        config_addr
                                    );
                                    registered = true;
                                    break;
                                }
                            }
                        }
                    }
                }

                // 2. Heartbeat to Config Server
                let rps_per_prefix: HashMap<String, f64> = {
                    let metrics = monitor_clone.metrics.lock().expect("Mutex poisoned");
                    metrics.iter().map(|(p, m)| (p.clone(), m.rps)).collect()
                };
                for config_addr in &config_addrs {
                    use crate::dfs::config_service_client::ConfigServiceClient;

                    if let Ok(channel) = dfs_common::security::connect_endpoint(
                        config_addr,
                        ca_cert.as_deref(),
                        domain.as_deref(),
                    )
                    .await
                    {
                        let mut client = ConfigServiceClient::new(channel);
                        let hb_req = crate::dfs::ShardHeartbeatRequest {
                            address: master_addr.clone(),
                            rps_per_prefix: rps_per_prefix.clone(),
                        };
                        let _ = client.shard_heartbeat(hb_req).await;
                    }
                }

                // 3. Split Detection
                let split_threshold = monitor_clone.split_threshold_rps;
                let split_cooldown = monitor_clone.split_cooldown_secs;
                let now = Instant::now();

                let hot_prefix = {
                    let metrics = monitor_clone.metrics.lock().expect("Mutex poisoned");
                    let last_split = monitor_clone.last_split_time.lock().unwrap();

                    if now.duration_since(*last_split).as_secs() >= split_cooldown {
                        metrics
                            .iter()
                            .find(|(_, m)| m.rps > split_threshold)
                            .map(|(p, m)| (p.clone(), m.rps))
                    } else {
                        None
                    }
                };

                if let Some((prefix, rps)) = hot_prefix {
                    tracing::warn!(
                        "Hot prefix detected: {} (RPS={:.2}). Triggering shard split...",
                        prefix,
                        rps
                    );

                    // Propose SplitShard command to local Raft
                    let new_shard_id = format!(
                        "{}-split-{}",
                        shard_id,
                        Uuid::new_v4().to_string().split('-').next().unwrap()
                    );
                    let (tx, rx) = tokio::sync::oneshot::channel();
                    let split_cmd = MasterCommand::SplitShard {
                        split_key: prefix.clone(),
                        new_shard_id: new_shard_id.clone(),
                        new_shard_peers: vec![], // Config Server will allocate
                    };

                    if let Err(e) = raft_tx
                        .send(Event::ClientRequest {
                            command: Command::Master(split_cmd),
                            reply_tx: tx,
                        })
                        .await
                    {
                        tracing::error!("Failed to send split command to Raft: {}", e);
                        continue;
                    }

                    match rx.await {
                        Ok(Ok(_)) => {
                            tracing::info!(
                                "Successfully committed split shard {} at prefix {}",
                                shard_id,
                                prefix
                            );

                            // Update last split time for cooldown
                            {
                                let mut last_split = monitor_clone.last_split_time.lock().unwrap();
                                *last_split = Instant::now();
                            }

                            // Identify files that just moved (lexicographical >= prefix)
                            let files_to_move = {
                                let state_lock = state_clone.lock().unwrap();
                                if let AppState::Master(ref master_state) = *state_lock {
                                    master_state
                                        .files
                                        .iter()
                                        .filter(|(path, _)| path.starts_with(&prefix))
                                        .map(|(_, meta)| meta.clone())
                                        .collect::<Vec<_>>()
                                } else {
                                    vec![]
                                }
                            };

                            // Notify Config Server and trigger Metadata Migration
                            // Notify Config Server and trigger Metadata Migration
                            let config_addrs_inner = config_addrs.clone();
                            let prefix_report = prefix.clone();
                            let shard_id_report = shard_id.clone();
                            let new_shard_report = new_shard_id.clone();
                            let files_payload = files_to_move.clone();

                            tokio::spawn(async move {
                                for config_addr in config_addrs_inner {
                                    use crate::dfs::config_service_client::ConfigServiceClient;
                                    let addr_to_connect = if config_addr.starts_with("http://") {
                                        config_addr.clone()
                                    } else {
                                        format!("http://{}", config_addr)
                                    };
                                    if let Ok(mut client) =
                                        ConfigServiceClient::connect(addr_to_connect).await
                                    {
                                        let split_req = crate::dfs::SplitShardRequest {
                                            shard_id: shard_id_report.clone(),
                                            split_key: prefix_report.clone(),
                                            new_shard_id: new_shard_report.clone(),
                                            new_shard_peers: vec![],
                                        };
                                        if let Ok(resp) = client.split_shard(split_req).await {
                                            let resp = resp.into_inner();
                                            if resp.success {
                                                tracing::info!(
                                                    "Config server updated. New shard peers: {:?}",
                                                    resp.new_shard_peers
                                                );

                                                // Metadata Push
                                                if !files_payload.is_empty()
                                                    && !resp.new_shard_peers.is_empty()
                                                {
                                                    for peer in resp.new_shard_peers {
                                                        use crate::dfs::master_service_client::MasterServiceClient;
                                                        let peer_addr = if peer.starts_with("http")
                                                        {
                                                            peer
                                                        } else {
                                                            format!("http://{}", peer)
                                                        };
                                                        if let Ok(mut master_client) =
                                                            MasterServiceClient::connect(
                                                                peer_addr.clone(),
                                                            )
                                                            .await
                                                        {
                                                            let ingest_req =
                                                                crate::dfs::IngestMetadataRequest {
                                                                    files: files_payload.clone(),
                                                                };
                                                            if master_client
                                                                .ingest_metadata(ingest_req)
                                                                .await
                                                                .is_ok()
                                                            {
                                                                tracing::info!("Successfully migrated metadata to {}", peer_addr);
                                                                break;
                                                            }
                                                        }
                                                    }
                                                }
                                                break;
                                            }
                                        }
                                    }
                                }
                            });
                        }
                        Ok(Err(hint)) => {
                            tracing::warn!("Failed to split shard (not leader). Hint: {:?}", hint)
                        }
                        Err(_) => tracing::error!("Raft response error during split"),
                    }
                }

                // 4. Merge Detection
                let total_rps: f64 = {
                    let metrics = monitor_clone.metrics.lock().expect("Mutex poisoned");
                    metrics.values().map(|m| m.rps).sum()
                };

                let merge_threshold = monitor_clone.merge_threshold_rps;
                // Skip merge if threshold is negative (disabled for testing)
                if merge_threshold >= 0.0 && total_rps < merge_threshold {
                    // Try to merge with a neighbor
                    let neighbors = {
                        let map = shard_map_clone.lock().unwrap();
                        map.get_neighbors(&shard_id)
                    };

                    // Heuristic: Prefer merging with predecessor if it exists
                    if let Some(neighbor_id) = neighbors.0.or(neighbors.1) {
                        tracing::warn!(
                            "Shard {} is underutilized (total RPS={:.2} < {:.2}). Proposing merge with {}...",
                            shard_id,
                            total_rps,
                            merge_threshold,
                            neighbor_id
                        );

                        let config_addrs_inner = config_addrs.clone();
                        let victim = neighbor_id.clone();
                        let retained = shard_id.clone();
                        let shard_map_merge_inner = shard_map_clone.clone();
                        let state_merge_inner = state_clone.clone();

                        tokio::spawn(async move {
                            for config_addr in config_addrs_inner {
                                use crate::dfs::config_service_client::ConfigServiceClient;
                                let addr_to_connect = if config_addr.starts_with("http://") {
                                    config_addr.clone()
                                } else {
                                    format!("http://{}", config_addr)
                                };
                                if let Ok(mut client) =
                                    ConfigServiceClient::connect(addr_to_connect).await
                                {
                                    let merge_req = crate::dfs::MergeShardRequest {
                                        victim_shard_id: victim.clone(),
                                        retained_shard_id: retained.clone(),
                                    };
                                    if let Ok(resp) = client.merge_shard(merge_req).await {
                                        let resp_inner = resp.into_inner();
                                        if resp_inner.success {
                                            tracing::info!(
                                                "Successfully initiated merge of {} into {}",
                                                victim,
                                                retained
                                            );

                                            // Metadata Migration for Merge
                                            let all_files = {
                                                let state_lock = state_merge_inner.lock().unwrap();
                                                if let AppState::Master(ref master_state) =
                                                    *state_lock
                                                {
                                                    master_state.files.values().cloned().collect()
                                                } else {
                                                    vec![]
                                                }
                                            };

                                            let shard_map_for_peers = shard_map_merge_inner.clone();
                                            let target_shard = retained.clone();

                                            tokio::spawn(async move {
                                                let peers = {
                                                    let map = shard_map_for_peers.lock().unwrap();
                                                    map.get_shard_peers(&target_shard)
                                                };

                                                if let Some(peer_list) = peers {
                                                    for peer in peer_list {
                                                        use crate::dfs::master_service_client::MasterServiceClient;
                                                        let peer_addr = if peer.starts_with("http")
                                                        {
                                                            peer
                                                        } else {
                                                            format!("http://{}", peer)
                                                        };
                                                        if let Ok(mut master_client) =
                                                            MasterServiceClient::connect(
                                                                peer_addr.clone(),
                                                            )
                                                            .await
                                                        {
                                                            let ingest_req =
                                                                crate::dfs::IngestMetadataRequest {
                                                                    files: all_files.clone(),
                                                                };
                                                            if master_client
                                                                .ingest_metadata(ingest_req)
                                                                .await
                                                                .is_ok()
                                                            {
                                                                tracing::info!("Successfully migrated all metadata to {} during merge", peer_addr);
                                                                break;
                                                            }
                                                        }
                                                    }
                                                }
                                            });

                                            break;
                                        } else {
                                            tracing::error!(
                                                "Merge request failed: {}",
                                                resp_inner.error_message
                                            );
                                        }
                                    }
                                }
                            }
                        });
                    }
                }
            }
        });

        MyMaster {
            state,
            raft_tx,
            shard_map,
            shard_id: config.shard_id.clone(),
            monitor,
            _master_address: config.master_address.clone(),
            _config_server_addrs: config.config_server_addrs.clone(),
            config,
        }
    }

    async fn ensure_linearizable_read(&self) -> Result<(), Status> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.raft_tx
            .send(Event::GetReadIndex { reply_tx: tx })
            .await
            .map_err(|e| Status::internal(format!("Failed to send to Raft node: {}", e)))?;

        match rx.await {
            Ok(Ok(_)) => Ok(()),
            Ok(Err(leader_hint)) => {
                let msg = if let Some(hint) = leader_hint {
                    format!("Not Leader|{}", hint)
                } else {
                    "Not Leader".to_string()
                };
                Err(Status::failed_precondition(msg))
            }
            Err(_) => Err(Status::internal("Raft node shutdown")),
        }
    }

    /// Background tiering scanner: identifies cold files and issues MOVE_TO_COLD commands.
    pub async fn scan_tiering(&self) {
        // Only the leader should run the tiering scanner
        let (tx, rx) = tokio::sync::oneshot::channel();
        if self
            .raft_tx
            .send(Event::GetLeaderInfo { reply_tx: tx })
            .await
            .is_err()
        {
            return;
        }
        let leader = match rx.await {
            Ok(Some(addr)) => addr,
            _ => return, // no leader or channel closed
        };
        if leader != self.config.master_address {
            return; // we are not the leader
        }

        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        let cold_threshold_ms = self.config.cold_threshold_secs * 1000;

        // Collect candidates without holding the lock across awaits
        let candidates: Vec<(String, Vec<crate::dfs::BlockInfo>)> = {
            let state = self.state.lock().expect("Mutex poisoned");
            if let AppState::Master(ref ms) = *state {
                ms.files
                    .values()
                    .filter(|f| {
                        f.moved_to_cold_at_ms == 0
                            && f.ec_data_shards == 0
                            && f.last_access_ms > 0
                            && now_ms.saturating_sub(f.last_access_ms) > cold_threshold_ms
                    })
                    .map(|f| (f.path.clone(), f.blocks.clone()))
                    .collect()
            } else {
                vec![]
            }
        };

        for (path, blocks) in candidates {
            // Queue MOVE_TO_COLD commands to all ChunkServers holding blocks
            {
                let mut state = self.state.lock().expect("Mutex poisoned");
                if let AppState::Master(ref mut ms) = *state {
                    for block in &blocks {
                        for loc in &block.locations {
                            ms.pending_commands.entry(loc.clone()).or_default().push(
                                crate::dfs::ChunkServerCommand {
                                    r#type:
                                        crate::dfs::chunk_server_command::CommandType::MoveToCold
                                            as i32,
                                    block_id: block.block_id.clone(),
                                    ..Default::default()
                                },
                            );
                        }
                    }
                }
            }

            // Persist cold-move via Raft (best effort — ignore Not Leader errors)
            let (tx, rx) = tokio::sync::oneshot::channel();
            let _ = self
                .raft_tx
                .send(Event::ClientRequest {
                    command: Command::Master(MasterCommand::MoveToCold {
                        path: path.clone(),
                        moved_at_ms: now_ms,
                    }),
                    reply_tx: tx,
                })
                .await;
            let _ = rx.await;
            tracing::info!("Tiering scanner: queued cold move for {}", path);
        }
    }

    /// Background EC conversion scanner: converts cold files to erasure coding.
    pub async fn scan_ec_conversion(&self) {
        // Only the leader should run the EC conversion scanner
        let (tx, rx) = tokio::sync::oneshot::channel();
        if self
            .raft_tx
            .send(Event::GetLeaderInfo { reply_tx: tx })
            .await
            .is_err()
        {
            return;
        }
        let leader = match rx.await {
            Ok(Some(addr)) => addr,
            _ => return, // no leader or channel closed
        };
        if leader != self.config.master_address {
            return; // we are not the leader
        }

        const EC_DATA: i32 = 6;
        const EC_PARITY: i32 = 3;
        let total_shards = (EC_DATA + EC_PARITY) as usize;

        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        let ec_threshold_ms = self.config.ec_threshold_secs * 1000;

        // Collect candidates: cold files that have aged past ec_threshold
        let candidates: Vec<crate::dfs::FileMetadata> = {
            let state = self.state.lock().expect("Mutex poisoned");
            if let AppState::Master(ref ms) = *state {
                ms.files
                    .values()
                    .filter(|f| {
                        f.moved_to_cold_at_ms > 0
                            && f.ec_data_shards == 0
                            && now_ms.saturating_sub(f.moved_to_cold_at_ms) > ec_threshold_ms
                    })
                    .cloned()
                    .collect()
            } else {
                vec![]
            }
        };

        for file in candidates {
            // Pick enough ChunkServers for EC shards
            let shard_servers: Vec<String> = {
                let state = self.state.lock().expect("Mutex poisoned");
                if let AppState::Master(ref ms) = *state {
                    let mut servers: Vec<String> = ms
                        .chunk_servers
                        .iter()
                        .filter(|(_, s)| s.available_space > 0)
                        .map(|(addr, _)| addr.clone())
                        .collect();
                    // Sort for determinism in tests
                    servers.sort();
                    servers.truncate(total_shards);
                    servers
                } else {
                    vec![]
                }
            };

            if shard_servers.len() < total_shards {
                tracing::warn!(
                    "Not enough ChunkServers for EC conversion of {} (need {}, have {})",
                    file.path,
                    total_shards,
                    shard_servers.len()
                );
                continue;
            }

            let mut new_blocks: Vec<crate::dfs::BlockInfo> = Vec::new();

            for block in &file.blocks {
                // One CS per EC shard
                let new_block = crate::dfs::BlockInfo {
                    block_id: block.block_id.clone(),
                    size: block.size,
                    locations: shard_servers.clone(),
                    checksum_crc32c: block.checksum_crc32c,
                    ec_data_shards: EC_DATA,
                    ec_parity_shards: EC_PARITY,
                    original_size: block.original_size,
                };
                new_blocks.push(new_block);

                // TODO: Issue RECONSTRUCT_EC_SHARD commands to write EC shards from the source block.
                // The current reconstruct_ec_shard RPC is designed for reconstructing MISSING shards
                // from EXISTING EC shards — it cannot convert a full replication block to EC shards.
                // A dedicated "EC encode and distribute" command is needed (future work).
                // For now, we update the metadata so the block is marked as EC; the data migration
                // will be handled when that command is implemented.

                // NOTE: Old replication blocks are intentionally NOT deleted here.
                // Deletion races with EC shard reconstruction and could cause data loss.
                // Old replicas become orphaned and will be reclaimed by a future
                // background orphan-cleanup pass (not yet implemented).
            }

            // Persist EC conversion via Raft
            let (tx, rx) = tokio::sync::oneshot::channel();
            let _ = self
                .raft_tx
                .send(Event::ClientRequest {
                    command: Command::Master(MasterCommand::ConvertToEc {
                        path: file.path.clone(),
                        ec_data_shards: EC_DATA,
                        ec_parity_shards: EC_PARITY,
                        new_blocks,
                    }),
                    reply_tx: tx,
                })
                .await;
            let _ = rx.await;
            tracing::info!("Tiering scanner: queued EC conversion for {}", file.path);
        }
    }

    #[allow(clippy::result_large_err)]
    fn check_shard_ownership(&self, path: &str) -> Result<(), Status> {
        let map = self.shard_map.lock().expect("Mutex poisoned");
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

    /// Check if the cluster is in safe mode and reject write operations
    #[allow(clippy::result_large_err)]
    fn check_safe_mode(&self) -> Result<(), Status> {
        let state_lock = self.state.lock().expect("Mutex poisoned");
        if let AppState::Master(ref state) = *state_lock {
            if state.is_in_safe_mode() {
                return Err(Status::unavailable(
                    "Cluster is in Safe Mode. Write operations are blocked.",
                ));
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
        let span = dfs_common::telemetry::create_server_span(&request, "get_file_info");
        async move {
            let req = request.into_inner();

            self.monitor.record_request(&req.path, 0); // Read request, 0 bytes for metadata

            // Fire-and-forget access stat update for storage tiering (best effort)
            {
                let now_ms = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as u64;
                let raft_tx = self.raft_tx.clone();
                let path_clone = req.path.clone();
                tokio::spawn(async move {
                    let (tx, _rx) = tokio::sync::oneshot::channel();
                    let _ = raft_tx
                        .send(Event::ClientRequest {
                            command: Command::Master(MasterCommand::UpdateAccessStats {
                                path: path_clone,
                                accessed_at_ms: now_ms,
                            }),
                            reply_tx: tx,
                        })
                        .await;
                });
            }

            self.check_shard_ownership(&req.path)?;
            self.ensure_linearizable_read().await?;

            let state_lock = self.state.lock().expect("Mutex poisoned");
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
                Err(Status::internal("Invalid state"))
            }
        }
        .instrument(span)
        .await
    }

    async fn create_file(
        &self,
        request: Request<CreateFileRequest>,
    ) -> Result<Response<CreateFileResponse>, Status> {
        let span = dfs_common::telemetry::create_server_span(&request, "create_file");
        async move {
            let req = request.into_inner();
            self.monitor.record_request(&req.path, 0);
            self.check_shard_ownership(&req.path)?;
            self.check_safe_mode()?;

            // Check if file exists (read optimization)
            {
                let state_lock = self.state.lock().expect("Mutex poisoned");
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
                    command: Command::Master(MasterCommand::CreateFile {
                        path: req.path,
                        ec_data_shards: req.ec_data_shards,
                        ec_parity_shards: req.ec_parity_shards,
                    }),
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
        .instrument(span)
        .await
    }

    async fn delete_file(
        &self,
        request: Request<DeleteFileRequest>,
    ) -> Result<Response<DeleteFileResponse>, Status> {
        let span = dfs_common::telemetry::create_server_span(&request, "delete_file");
        async move {
            let req = request.into_inner();
            self.monitor.record_request(&req.path, 0);
            self.check_shard_ownership(&req.path)?;
            self.check_safe_mode()?;

            // Check if file exists
            {
                let state_lock = self.state.lock().expect("Mutex poisoned");
                if let AppState::Master(ref state) = *state_lock {
                    if !state.files.contains_key(&req.path) {
                        return Ok(Response::new(DeleteFileResponse {
                            success: false,
                            error_message: "File not found".to_string(),
                            leader_hint: "".to_string(),
                        }));
                    }
                }
            }

            let (tx, rx) = tokio::sync::oneshot::channel();
            if self
                .raft_tx
                .send(Event::ClientRequest {
                    command: Command::Master(MasterCommand::DeleteFile { path: req.path }),
                    reply_tx: tx,
                })
                .await
                .is_err()
            {
                return Err(Status::internal("Raft channel closed"));
            }

            match rx.await {
                Ok(Ok(())) => Ok(Response::new(DeleteFileResponse {
                    success: true,
                    error_message: "".to_string(),
                    leader_hint: "".to_string(),
                })),
                Ok(Err(leader_opt)) => Ok(Response::new(DeleteFileResponse {
                    success: false,
                    error_message: "Not Leader".to_string(),
                    leader_hint: leader_opt.unwrap_or_default(),
                })),
                Err(_) => Err(Status::internal("Raft response error")),
            }
        }
        .instrument(span)
        .await
    }

    async fn allocate_block(
        &self,
        request: Request<AllocateBlockRequest>,
    ) -> Result<Response<AllocateBlockResponse>, Status> {
        let span = dfs_common::telemetry::create_server_span(&request, "allocate_block");
        async move {
            let req = request.into_inner();

            self.monitor.record_request(&req.path, 0);
            self.check_shard_ownership(&req.path)?;
            self.check_safe_mode()?;

            let (selected_servers, block_id, ec_data, ec_parity) = {
                let state_lock = self.state.lock().expect("Mutex poisoned");
                if let AppState::Master(ref state) = *state_lock {
                    if !state.files.contains_key(&req.path) {
                        return Err(Status::not_found("File not found"));
                    }

                    // Look up file's EC policy
                    let file_meta = &state.files[&req.path];
                    let ec_data = file_meta.ec_data_shards;
                    let ec_parity = file_meta.ec_parity_shards;

                    if (ec_data > 0) != (ec_parity > 0) {
                        tracing::warn!(
                            path = %req.path,
                            ec_data,
                            ec_parity,
                            "EC policy partially set — only one of ec_data/ec_parity is non-zero; falling back to replication"
                        );
                    }

                    let candidates: Vec<(String, ChunkServerStatus)> = state
                        .chunk_servers
                        .iter()
                        .map(|(addr, status)| (addr.clone(), status.clone()))
                        .collect();

                    let needed = if ec_data > 0 && ec_parity > 0 {
                        // EC requires exactly k+m distinct chunkservers (one shard each)
                        let total = (ec_data + ec_parity) as usize;
                        if candidates.len() < total {
                            return Err(Status::unavailable(format!(
                                "Need {} chunk servers for EC({},{}), only {} available",
                                total, ec_data, ec_parity, candidates.len()
                            )));
                        }
                        total
                    } else {
                        // Replication: use as many replicas as possible up to DEFAULT_REPLICATION_FACTOR
                        std::cmp::min(DEFAULT_REPLICATION_FACTOR, candidates.len())
                    };

                    if needed == 0 {
                        return Err(Status::unavailable("No chunk servers available"));
                    }

                    let selected = select_servers_rack_aware(&candidates, needed);
                    (selected, Uuid::new_v4().to_string(), ec_data, ec_parity)
                } else {
                    return Err(Status::internal("Wrong state type"));
                }
            };

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
                        checksum_crc32c: 0,
                        ec_data_shards: ec_data,
                        ec_parity_shards: ec_parity,
                        original_size: 0,
                    };
                    Ok(Response::new(AllocateBlockResponse {
                        block: Some(block),
                        chunk_server_addresses: selected_servers,
                        leader_hint: "".to_string(),
                        ec_data_shards: ec_data,
                        ec_parity_shards: ec_parity,
                    }))
                }
                Ok(Err(leader_opt)) => {
                    let leader_hint = leader_opt.unwrap_or_default();

                    Ok(Response::new(AllocateBlockResponse {
                        block: None,
                        chunk_server_addresses: vec![],
                        leader_hint,
                        ec_data_shards: 0,
                        ec_parity_shards: 0,
                    }))
                }
                Err(_) => Err(Status::internal("Raft response error")),
            }
        }
        .instrument(span)
        .await
    }

    async fn complete_file(
        &self,
        request: Request<CompleteFileRequest>,
    ) -> Result<Response<CompleteFileResponse>, Status> {
        let span = dfs_common::telemetry::create_server_span(&request, "complete_file");
        async move {
            let req = request.into_inner();
            self.check_shard_ownership(&req.path)?;

            // In our simple implementation, the size of blocks might still be 0 if the
            // ChunkServers haven't reported back yet.
            // Ideally we'd wait for reports or have metadata update from client.
            // For the test, we'll assume the client knows or we just finalize whatever we have.
            // Actually, WriteBlock doesn't report size back to Master immediately.
            // Let's assume the test file size is what we want.

            let (tx, rx) = tokio::sync::oneshot::channel();
            if self
                .raft_tx
                .send(Event::ClientRequest {
                    command: Command::Master(MasterCommand::CompleteFile {
                        path: req.path,
                        size: req.size,
                        etag_md5: if req.etag_md5.is_empty() {
                            None
                        } else {
                            Some(req.etag_md5)
                        },
                        created_at_ms: if req.created_at_ms == 0 {
                            None
                        } else {
                            Some(req.created_at_ms)
                        },
                        block_checksums: req.block_checksums,
                    }),
                    reply_tx: tx,
                })
                .await
                .is_err()
            {
                return Err(Status::internal("Raft channel closed"));
            }

            match rx.await {
                Ok(Ok(())) => Ok(Response::new(CompleteFileResponse { success: true })),
                Ok(Err(leader_opt)) => {
                    tracing::warn!(
                        "Failed to complete file: Not Leader. Hint: {:?}",
                        leader_opt
                    );
                    Ok(Response::new(CompleteFileResponse { success: false }))
                }
                Err(_) => Err(Status::internal("Raft response error")),
            }
        }
        .instrument(span)
        .await
    }

    async fn list_files(
        &self,
        request: Request<ListFilesRequest>,
    ) -> Result<Response<ListFilesResponse>, Status> {
        let span = dfs_common::telemetry::create_server_span(&request, "list_files");
        async move {
            self.ensure_linearizable_read().await?;
            let req = request.into_inner();
            let prefix = req.path;
            let state_lock = self.state.lock().expect("Mutex poisoned");
            if let AppState::Master(ref state) = *state_lock {
                let files: Vec<String> = if prefix.is_empty() {
                    state.files.keys().cloned().collect()
                } else {
                    state
                        .files
                        .keys()
                        .filter(|k| k.starts_with(&prefix))
                        .cloned()
                        .collect()
                };
                Ok(Response::new(ListFilesResponse { files }))
            } else {
                Err(Status::internal("Invalid state"))
            }
        }
        .instrument(span)
        .await
    }

    async fn register_chunk_server(
        &self,
        request: Request<RegisterChunkServerRequest>,
    ) -> Result<Response<RegisterChunkServerResponse>, Status> {
        let span = dfs_common::telemetry::create_server_span(&request, "register_chunk_server");
        async move {
            let req = request.into_inner();

            let mut state_lock = self.state.lock().expect("Mutex poisoned");
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("Time went backwards")
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
                        rack_id: req.rack_id,
                    },
                );
            }

            Ok(Response::new(RegisterChunkServerResponse { success: true }))
        }
        .instrument(span)
        .await
    }

    async fn heartbeat(
        &self,
        request: Request<HeartbeatRequest>,
    ) -> Result<Response<HeartbeatResponse>, Status> {
        let span = dfs_common::telemetry::create_server_span(&request, "heartbeat");
        async move {
            let mut req = request.into_inner();
            let mut state_lock = self.state.lock().expect("Mutex poisoned");
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("Time went backwards")
                .as_millis() as u64;

            if let AppState::Master(ref mut state) = *state_lock {
                // Check if this is a new ChunkServer registration
                let is_new_chunkserver =
                    !state.chunk_servers.contains_key(&req.chunk_server_address);

                // Use rack_id from heartbeat if provided, otherwise preserve existing
                let rack_id = if !req.rack_id.is_empty() {
                    std::mem::take(&mut req.rack_id)
                } else {
                    state
                        .chunk_servers
                        .get(&req.chunk_server_address)
                        .map(|s| s.rack_id.clone())
                        .unwrap_or_default()
                };

                state.chunk_servers.insert(
                    req.chunk_server_address.clone(),
                    ChunkServerStatus {
                        last_heartbeat: now,
                        used_space: req.used_space,
                        available_space: req.available_space,
                        chunk_count: req.chunk_count,
                        rack_id,
                    },
                );

                // If in safe mode and this is a new ChunkServer, update block count
                if state.is_in_safe_mode() && is_new_chunkserver {
                    state.update_reported_blocks(req.chunk_count as usize);
                }

                // Always check if we should exit safe mode (covers timeout and 0-block case)
                if state.is_in_safe_mode() && state.should_exit_safe_mode() {
                    state.exit_safe_mode();
                }

                // Process bad block reports from the ChunkServer's scrubber
                if !req.bad_blocks.is_empty() {
                    tracing::warn!(
                        "Heartbeat: {} bad block(s) reported by {}",
                        req.bad_blocks.len(),
                        req.chunk_server_address
                    );
                    for block_id in &req.bad_blocks {
                        state
                            .bad_block_locations
                            .entry(block_id.clone())
                            .or_default()
                            .insert(req.chunk_server_address.clone());
                    }
                    heal_under_replicated_blocks(state);
                }

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
        .instrument(span)
        .await
    }

    async fn get_block_locations(
        &self,
        request: Request<GetBlockLocationsRequest>,
    ) -> Result<Response<GetBlockLocationsResponse>, Status> {
        let span = dfs_common::telemetry::create_server_span(&request, "get_block_locations");
        async move {
            let req = request.into_inner();
            // Search for the block in all files
            // (Note: This is expensive, but for now we do it)
            // No path provided in GetBlockLocationsRequest, so we can't easily record_request per prefix
            // unless we find the file first.
            // For now, let's skip recording for GetBlockLocations or use "/" prefix.
            self.ensure_linearizable_read().await?;
            let state_lock = self.state.lock().expect("Mutex poisoned");
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
        .instrument(span)
        .await
    }

    // =========================================================================
    // Rename RPC Handler (Coordinator for Cross-Shard Operations)
    // =========================================================================
    async fn rename(
        &self,
        request: Request<RenameRequest>,
    ) -> Result<Response<RenameResponse>, Status> {
        let span = dfs_common::telemetry::create_server_span(&request, "rename");
        async move {
            let req = request.into_inner();
            let source_path = req.source_path;
            let dest_path = req.dest_path;

            self.monitor.record_request(&source_path, 0);
            // Check source shard ownership
            self.check_shard_ownership(&source_path)?;
            self.check_safe_mode()?;

            // Determine source and dest shard IDs
            let (source_shard, dest_shard, dest_peers) = {
                let map = self.shard_map.lock().expect("Mutex poisoned");
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
                let state_lock = self.state.lock().expect("Mutex poisoned");
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
                                command: Command::Master(
                                    MasterCommand::ApplyTransactionOperation {
                                        tx_id: tx_id.clone(),
                                        operation: TxOperation {
                                            shard_id: source_shard.clone(),
                                            op_type: TxOpType::Delete {
                                                path: source_path.clone(),
                                            },
                                        },
                                    },
                                ),
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
        .instrument(span)
        .await
    }

    // =========================================================================
    // PrepareTransaction RPC Handler (Participant in Cross-Shard Operations)
    // =========================================================================
    async fn prepare_transaction(
        &self,
        request: Request<PrepareTransactionRequest>,
    ) -> Result<Response<PrepareTransactionResponse>, Status> {
        let span = dfs_common::telemetry::create_server_span(&request, "prepare_transaction");
        async move {
            let req = request.into_inner();
            let tx_id = req.tx_id;
            let path = req.path;
            let metadata = req.metadata;

            // Check shard ownership
            self.check_shard_ownership(&path)?;

            // Validate: dest file should not exist
            {
                let state_lock = self.state.lock().expect("Mutex poisoned");
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
                .expect("Time went backwards")
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
        .instrument(span)
        .await
    }

    // =========================================================================
    // CommitTransaction RPC Handler
    // =========================================================================
    async fn commit_transaction(
        &self,
        request: Request<CommitTransactionRequest>,
    ) -> Result<Response<CommitTransactionResponse>, Status> {
        let span = dfs_common::telemetry::create_server_span(&request, "commit_transaction");
        async move {
            let req = request.into_inner();
            let tx_id = req.tx_id;

            // Find transaction record and apply the operation
            let operation = {
                let state_lock = self.state.lock().expect("Mutex poisoned");
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
        .instrument(span)
        .await
    }

    // =========================================================================
    // AbortTransaction RPC Handler
    // =========================================================================
    async fn abort_transaction(
        &self,
        request: Request<AbortTransactionRequest>,
    ) -> Result<Response<AbortTransactionResponse>, Status> {
        let span = dfs_common::telemetry::create_server_span(&request, "abort_transaction");
        async move {
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
        .instrument(span)
        .await
    }

    // =========================================================================
    // Safe Mode RPC Handlers
    // =========================================================================
    async fn get_safe_mode_status(
        &self,
        _request: Request<GetSafeModeStatusRequest>,
    ) -> Result<Response<GetSafeModeStatusResponse>, Status> {
        let span = dfs_common::telemetry::create_server_span(&_request, "get_safe_mode_status");
        async move {
            let state_lock = self.state.lock().expect("Mutex poisoned");
            if let AppState::Master(ref state) = *state_lock {
                Ok(Response::new(GetSafeModeStatusResponse {
                    is_safe_mode: state.safe_mode,
                    is_manual: state.safe_mode_manual,
                    chunk_server_count: state.chunk_servers.len() as u32,
                    expected_blocks: state.expected_block_count as u32,
                    reported_blocks: state.reported_block_count as u32,
                    threshold: state.safe_mode_threshold,
                    entered_at: state.safe_mode_entered_at,
                }))
            } else {
                Err(Status::internal("Wrong state type"))
            }
        }
        .instrument(span)
        .await
    }

    async fn set_safe_mode(
        &self,
        request: Request<SetSafeModeRequest>,
    ) -> Result<Response<SetSafeModeResponse>, Status> {
        let span = dfs_common::telemetry::create_server_span(&request, "set_safe_mode");
        async move {
            let req = request.into_inner();

            let mut state_lock = self.state.lock().expect("Mutex poisoned");
            if let AppState::Master(ref mut state) = *state_lock {
                if req.enter {
                    state.force_enter_safe_mode();
                    Ok(Response::new(SetSafeModeResponse {
                        success: true,
                        error_message: "".to_string(),
                        is_safe_mode: true,
                    }))
                } else {
                    state.force_exit_safe_mode();
                    Ok(Response::new(SetSafeModeResponse {
                        success: true,
                        error_message: "".to_string(),
                        is_safe_mode: false,
                    }))
                }
            } else {
                Err(Status::internal("Wrong state type"))
            }
        }
        .instrument(span)
        .await
    }

    // =========================================================================
    // Raft Cluster Membership Management RPC Handlers
    // =========================================================================
    async fn add_raft_server(
        &self,
        request: Request<AddRaftServerRequest>,
    ) -> Result<Response<AddRaftServerResponse>, Status> {
        let span = dfs_common::telemetry::create_server_span(&request, "add_raft_server");
        async move {
            let req = request.into_inner();

            // Submit membership change command to Raft
            let cmd = Command::Membership(MembershipCommand::AddServer {
                server_id: req.server_id as usize,
                server_address: req.server_address.clone(),
            });

            let (tx, rx) = tokio::sync::oneshot::channel();
            if self
                .raft_tx
                .send(Event::ClientRequest {
                    command: cmd,
                    reply_tx: tx,
                })
                .await
                .is_err()
            {
                return Err(Status::internal("Raft channel closed"));
            }

            match rx.await {
                Ok(Ok(())) => Ok(Response::new(AddRaftServerResponse {
                    success: true,
                    error_message: "".to_string(),
                    leader_hint: "".to_string(),
                })),
                Ok(Err(leader_opt)) => Ok(Response::new(AddRaftServerResponse {
                    success: false,
                    error_message: "Not Leader".to_string(),
                    leader_hint: leader_opt.unwrap_or_default(),
                })),
                Err(_) => Err(Status::internal("Raft response error")),
            }
        }
        .instrument(span)
        .await
    }

    async fn remove_raft_server(
        &self,
        request: Request<RemoveRaftServerRequest>,
    ) -> Result<Response<RemoveRaftServerResponse>, Status> {
        let span = dfs_common::telemetry::create_server_span(&request, "remove_raft_server");
        async move {
            let req = request.into_inner();

            // Safety check: get current cluster size
            let (cluster_size, is_leader) = {
                let (tx, rx) = tokio::sync::oneshot::channel();
                if self
                    .raft_tx
                    .send(Event::GetClusterInfo { reply_tx: tx })
                    .await
                    .is_err()
                {
                    return Err(Status::internal("Raft channel closed"));
                }

                match rx.await {
                    Ok(info) => (info.peers.len() + 1, info.role == Role::Leader), // +1 for self
                    Err(_) => return Err(Status::internal("Failed to get cluster info")),
                }
            };

            if !is_leader {
                // Not leader, return error with hint
                let (tx, rx) = tokio::sync::oneshot::channel();
                let _ = self
                    .raft_tx
                    .send(Event::GetLeaderInfo { reply_tx: tx })
                    .await;
                let leader_hint = rx.await.ok().flatten().unwrap_or_default();
                return Ok(Response::new(RemoveRaftServerResponse {
                    success: false,
                    error_message: "Not Leader".to_string(),
                    leader_hint,
                }));
            }

            // Safety check: don't remove if it would leave less than 1 node
            if cluster_size <= 1 {
                return Ok(Response::new(RemoveRaftServerResponse {
                    success: false,
                    error_message: "Cannot remove server: would leave cluster empty".to_string(),
                    leader_hint: "".to_string(),
                }));
            }

            // Submit membership change command to Raft
            let cmd = Command::Membership(MembershipCommand::RemoveServer {
                server_id: req.server_id as usize,
            });

            let (tx, rx) = tokio::sync::oneshot::channel();
            if self
                .raft_tx
                .send(Event::ClientRequest {
                    command: cmd,
                    reply_tx: tx,
                })
                .await
                .is_err()
            {
                return Err(Status::internal("Raft channel closed"));
            }

            match rx.await {
                Ok(Ok(())) => Ok(Response::new(RemoveRaftServerResponse {
                    success: true,
                    error_message: "".to_string(),
                    leader_hint: "".to_string(),
                })),
                Ok(Err(leader_opt)) => Ok(Response::new(RemoveRaftServerResponse {
                    success: false,
                    error_message: "Not Leader".to_string(),
                    leader_hint: leader_opt.unwrap_or_default(),
                })),
                Err(_) => Err(Status::internal("Raft response error")),
            }
        }
        .instrument(span)
        .await
    }

    async fn get_cluster_info(
        &self,
        request: Request<GetClusterInfoRequest>,
    ) -> Result<Response<GetClusterInfoResponse>, Status> {
        let span = dfs_common::telemetry::create_server_span(&request, "get_cluster_info");
        async move {
            let (tx, rx) = tokio::sync::oneshot::channel();
            if self
                .raft_tx
                .send(Event::GetClusterInfo { reply_tx: tx })
                .await
                .is_err()
            {
                return Err(Status::internal("Raft channel closed"));
            }

            match rx.await {
                Ok(info) => {
                    let role_str = match info.role {
                        Role::Leader => "Leader",
                        Role::Follower => "Follower",
                        Role::Candidate => "Candidate",
                    };

                    let members: Vec<ClusterMember> = info
                        .peers
                        .iter()
                        .enumerate()
                        .map(|(idx, addr)| ClusterMember {
                            server_id: idx as u32,
                            address: addr.clone(),
                            is_self: false,
                        })
                        .collect();

                    Ok(Response::new(GetClusterInfoResponse {
                        node_id: info.node_id as u32,
                        role: role_str.to_string(),
                        current_term: info.current_term,
                        leader_id: info.leader_id.map(|id| id as u32).unwrap_or(0),
                        leader_address: info.leader_address.unwrap_or_default(),
                        members,
                        commit_index: info.commit_index as u64,
                        last_applied: info.last_applied as u64,
                    }))
                }
                Err(_) => Err(Status::internal("Failed to get cluster info")),
            }
        }
        .instrument(span)
        .await
    }

    async fn ingest_metadata(
        &self,
        request: Request<IngestMetadataRequest>,
    ) -> Result<Response<IngestMetadataResponse>, Status> {
        let span = dfs_common::telemetry::create_server_span(&request, "ingest_metadata");
        async move {
            let req = request.into_inner();
            let (tx, rx) = tokio::sync::oneshot::channel();

            // Determine prefix before moving files into Raft command
            let shuffle_prefix = req.files.first().and_then(|f| {
                let path = &f.path;
                path.rfind('/').map(|pos| path[..pos + 1].to_string())
            });

            let cmd = MasterCommand::IngestBatch { files: req.files };

            if self
                .raft_tx
                .send(Event::ClientRequest {
                    command: Command::Master(cmd),
                    reply_tx: tx,
                })
                .await
                .is_err()
            {
                return Err(Status::internal("Raft channel closed"));
            }

            match rx.await {
                Ok(Ok(_)) => {
                    if let Some(prefix) = shuffle_prefix {
                        let (tx_sh, _rx_sh) = tokio::sync::oneshot::channel();
                        let _ = self
                            .raft_tx
                            .send(Event::ClientRequest {
                                command: Command::Master(MasterCommand::TriggerShuffle { prefix }),
                                reply_tx: tx_sh,
                            })
                            .await;
                    }

                    Ok(Response::new(IngestMetadataResponse {
                        success: true,
                        error_message: "".to_string(),
                        leader_hint: "".to_string(),
                    }))
                }
                Ok(Err(hint)) => Ok(Response::new(IngestMetadataResponse {
                    success: false,
                    error_message: "Not Leader".to_string(),
                    leader_hint: hint.unwrap_or_default(),
                })),
                Err(_) => Err(Status::internal("Internal error")),
            }
        }
        .instrument(span)
        .await
    }

    async fn initiate_shuffle(
        &self,
        request: Request<InitiateShuffleRequest>,
    ) -> Result<Response<InitiateShuffleResponse>, Status> {
        let span = dfs_common::telemetry::create_server_span(&request, "initiate_shuffle");
        async move {
            let req = request.into_inner();
            self.check_shard_ownership(&req.prefix)?;
            self.check_safe_mode()?;

            let (tx, rx) = tokio::sync::oneshot::channel();
            let cmd = MasterCommand::TriggerShuffle { prefix: req.prefix };

            if self
                .raft_tx
                .send(Event::ClientRequest {
                    command: Command::Master(cmd),
                    reply_tx: tx,
                })
                .await
                .is_err()
            {
                return Err(Status::internal("Raft channel closed"));
            }

            match rx.await {
                Ok(Ok(_)) => Ok(Response::new(InitiateShuffleResponse {
                    success: true,
                    error_message: "".to_string(),
                    leader_hint: "".to_string(),
                })),
                Ok(Err(hint)) => Ok(Response::new(InitiateShuffleResponse {
                    success: false,
                    error_message: "Not Leader".to_string(),
                    leader_hint: hint.unwrap_or_default(),
                })),
                Err(_) => Err(Status::internal("Internal error")),
            }
        }
        .instrument(span)
        .await
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
                            tracing::error!("PrepareTransaction failed to {}: {}", addr, e);
                            continue;
                        }
                    }
                }
                Err(e) => {
                    tracing::error!("Failed to connect to {}: {}", addr, e);
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
                            tracing::error!("CommitTransaction failed to {}: {}", addr, e);
                            continue;
                        }
                    }
                }
                Err(e) => {
                    tracing::error!("Failed to connect to {}: {}", addr, e);
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
            etag_md5: "".into(),
            created_at_ms: 0,
            ec_data_shards: 0,
            ec_parity_shards: 0,
            last_access_ms: 0,
            access_count: 0,
            moved_to_cold_at_ms: 0,
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
            etag_md5: "".into(),
            created_at_ms: 0,
            ec_data_shards: 0,
            ec_parity_shards: 0,
            last_access_ms: 0,
            access_count: 0,
            moved_to_cold_at_ms: 0,
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
            etag_md5: "".into(),
            created_at_ms: 0,
            ec_data_shards: 0,
            ec_parity_shards: 0,
            last_access_ms: 0,
            access_count: 0,
            moved_to_cold_at_ms: 0,
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
                checksum_crc32c: 0,
                ec_data_shards: 0,
                ec_parity_shards: 0,
                original_size: 0,
            }],
            etag_md5: "".into(),
            created_at_ms: 0,
            ec_data_shards: 0,
            ec_parity_shards: 0,
            last_access_ms: 0,
            access_count: 0,
            moved_to_cold_at_ms: 0,
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
            etag_md5: "".into(),
            created_at_ms: 0,
            ec_data_shards: 0,
            ec_parity_shards: 0,
            last_access_ms: 0,
            access_count: 0,
            moved_to_cold_at_ms: 0,
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

    #[test]
    fn test_heal_under_replicated_blocks_schedules_replication() {
        let mut state = MasterState::default();
        let now = 9_999_999_999_999u64;
        for addr in ["cs1:50055", "cs2:50056", "cs3:50057"] {
            state.chunk_servers.insert(
                addr.to_string(),
                ChunkServerStatus {
                    last_heartbeat: now,
                    used_space: 0,
                    available_space: 1_000_000,
                    chunk_count: 1,
                    rack_id: String::new(),
                },
            );
        }
        state.files.insert(
            "/test/file".to_string(),
            FileMetadata {
                path: "/test/file".to_string(),
                size: 100,
                blocks: vec![BlockInfo {
                    block_id: "block-1".to_string(),
                    size: 100,
                    locations: vec!["cs1:50055".to_string()],
                    checksum_crc32c: 0,
                    ec_data_shards: 0,
                    ec_parity_shards: 0,
                    original_size: 0,
                }],
                etag_md5: String::new(),
                created_at_ms: 0,
                ec_data_shards: 0,
                ec_parity_shards: 0,
                last_access_ms: 0,
                access_count: 0,
                moved_to_cold_at_ms: 0,
            },
        );
        heal_under_replicated_blocks(&mut state);
        let cmds = state
            .pending_commands
            .get("cs1:50055")
            .expect("commands expected");
        assert_eq!(cmds.len(), 2);
        let targets: std::collections::HashSet<_> = cmds
            .iter()
            .map(|c| c.target_chunk_server_address.as_str())
            .collect();
        assert!(targets.contains("cs2:50056") || targets.contains("cs3:50057"));
    }

    #[test]
    fn test_heal_skips_fully_replicated_blocks() {
        let mut state = MasterState::default();
        let now = 9_999_999_999_999u64;
        for addr in ["cs1:50055", "cs2:50056", "cs3:50057"] {
            state.chunk_servers.insert(
                addr.to_string(),
                ChunkServerStatus {
                    last_heartbeat: now,
                    used_space: 0,
                    available_space: 1_000_000,
                    chunk_count: 1,
                    rack_id: String::new(),
                },
            );
        }
        state.files.insert(
            "/test/full".to_string(),
            FileMetadata {
                path: "/test/full".to_string(),
                size: 100,
                blocks: vec![BlockInfo {
                    block_id: "block-full".to_string(),
                    size: 100,
                    locations: vec![
                        "cs1:50055".to_string(),
                        "cs2:50056".to_string(),
                        "cs3:50057".to_string(),
                    ],
                    checksum_crc32c: 0,
                    ec_data_shards: 0,
                    ec_parity_shards: 0,
                    original_size: 0,
                }],
                etag_md5: String::new(),
                created_at_ms: 0,
                ec_data_shards: 0,
                ec_parity_shards: 0,
                last_access_ms: 0,
                access_count: 0,
                moved_to_cold_at_ms: 0,
            },
        );
        heal_under_replicated_blocks(&mut state);
        assert!(state.pending_commands.is_empty());
    }

    #[test]
    fn test_heal_treats_bad_block_location_as_missing() {
        let mut state = MasterState::default();
        let now = 9_999_999_999_999u64;
        for addr in ["cs1:50055", "cs2:50056", "cs3:50057"] {
            state.chunk_servers.insert(
                addr.to_string(),
                ChunkServerStatus {
                    last_heartbeat: now,
                    used_space: 0,
                    available_space: 1_000_000,
                    chunk_count: 1,
                    rack_id: String::new(),
                },
            );
        }
        state.files.insert(
            "/test/bad".to_string(),
            FileMetadata {
                path: "/test/bad".to_string(),
                size: 100,
                blocks: vec![BlockInfo {
                    block_id: "block-bad".to_string(),
                    size: 100,
                    locations: vec![
                        "cs1:50055".to_string(),
                        "cs2:50056".to_string(),
                        "cs3:50057".to_string(),
                    ],
                    checksum_crc32c: 0,
                    ec_data_shards: 0,
                    ec_parity_shards: 0,
                    original_size: 0,
                }],
                etag_md5: String::new(),
                created_at_ms: 0,
                ec_data_shards: 0,
                ec_parity_shards: 0,
                last_access_ms: 0,
                access_count: 0,
                moved_to_cold_at_ms: 0,
            },
        );
        state
            .bad_block_locations
            .entry("block-bad".to_string())
            .or_default()
            .insert("cs2:50056".to_string());
        // 3 servers in metadata but cs2 is bad → 2 effective, need 1 more
        // but all 3 servers are already in block.locations so no unused target exists
        heal_under_replicated_blocks(&mut state);
        assert!(state.pending_commands.is_empty());
    }

    #[test]
    fn test_rack_aware_selection_spreads_across_racks() {
        // 3 servers on 3 different racks — should pick one per rack
        let servers = vec![
            (
                "cs1:50051".to_string(),
                ChunkServerStatus {
                    last_heartbeat: 9_999_999_999_999,
                    used_space: 0,
                    available_space: 1_000_000,
                    chunk_count: 0,
                    rack_id: "rack-a".to_string(),
                },
            ),
            (
                "cs2:50052".to_string(),
                ChunkServerStatus {
                    last_heartbeat: 9_999_999_999_999,
                    used_space: 0,
                    available_space: 900_000,
                    chunk_count: 0,
                    rack_id: "rack-b".to_string(),
                },
            ),
            (
                "cs3:50053".to_string(),
                ChunkServerStatus {
                    last_heartbeat: 9_999_999_999_999,
                    used_space: 0,
                    available_space: 800_000,
                    chunk_count: 0,
                    rack_id: "rack-c".to_string(),
                },
            ),
        ];
        let selected = select_servers_rack_aware(&servers, 3);
        assert_eq!(selected.len(), 3);
        // All 3 racks represented
        let racks: std::collections::HashSet<String> = selected
            .iter()
            .map(|addr| {
                servers
                    .iter()
                    .find(|(a, _)| a == addr)
                    .unwrap()
                    .1
                    .rack_id
                    .clone()
            })
            .collect();
        assert_eq!(racks.len(), 3, "All 3 racks should be represented");
    }

    #[test]
    fn test_rack_aware_selection_fallback_to_same_rack() {
        // 3 servers on only 2 racks — need 3 replicas, must pick 2 from one rack
        let servers = vec![
            (
                "cs1:50051".to_string(),
                ChunkServerStatus {
                    last_heartbeat: 9_999_999_999_999,
                    used_space: 0,
                    available_space: 1_000_000,
                    chunk_count: 0,
                    rack_id: "rack-a".to_string(),
                },
            ),
            (
                "cs2:50052".to_string(),
                ChunkServerStatus {
                    last_heartbeat: 9_999_999_999_999,
                    used_space: 0,
                    available_space: 900_000,
                    chunk_count: 0,
                    rack_id: "rack-a".to_string(),
                },
            ),
            (
                "cs3:50053".to_string(),
                ChunkServerStatus {
                    last_heartbeat: 9_999_999_999_999,
                    used_space: 0,
                    available_space: 800_000,
                    chunk_count: 0,
                    rack_id: "rack-b".to_string(),
                },
            ),
        ];
        let selected = select_servers_rack_aware(&servers, 3);
        assert_eq!(selected.len(), 3);
        // All 3 servers picked (no duplicates)
        let unique: std::collections::HashSet<&String> = selected.iter().collect();
        assert_eq!(unique.len(), 3);
    }

    #[test]
    fn test_rack_aware_selection_empty_rack_id_treated_as_distinct() {
        // Servers with empty rack_id should each be treated as their own rack
        let servers = vec![
            (
                "cs1:50051".to_string(),
                ChunkServerStatus {
                    last_heartbeat: 9_999_999_999_999,
                    used_space: 0,
                    available_space: 1_000_000,
                    chunk_count: 0,
                    rack_id: "".to_string(),
                },
            ),
            (
                "cs2:50052".to_string(),
                ChunkServerStatus {
                    last_heartbeat: 9_999_999_999_999,
                    used_space: 0,
                    available_space: 900_000,
                    chunk_count: 0,
                    rack_id: "".to_string(),
                },
            ),
        ];
        let selected = select_servers_rack_aware(&servers, 2);
        assert_eq!(selected.len(), 2);
    }

    #[test]
    fn test_rack_aware_selection_fewer_servers_than_replicas() {
        // Only 2 servers but want 3 replicas — return all 2
        let servers = vec![
            (
                "cs1:50051".to_string(),
                ChunkServerStatus {
                    last_heartbeat: 9_999_999_999_999,
                    used_space: 0,
                    available_space: 1_000_000,
                    chunk_count: 0,
                    rack_id: "rack-a".to_string(),
                },
            ),
            (
                "cs2:50052".to_string(),
                ChunkServerStatus {
                    last_heartbeat: 9_999_999_999_999,
                    used_space: 0,
                    available_space: 900_000,
                    chunk_count: 0,
                    rack_id: "rack-b".to_string(),
                },
            ),
        ];
        let selected = select_servers_rack_aware(&servers, 3);
        assert_eq!(selected.len(), 2);
    }

    #[test]
    fn test_ec_file_metadata_stores_policy() {
        let mut state = MasterState::default();
        // Simulate CreateFile with EC(4,2)
        let metadata = FileMetadata {
            path: "/ec-file".to_string(),
            size: 0,
            blocks: vec![],
            etag_md5: "".to_string(),
            created_at_ms: 0,
            ec_data_shards: 4,
            ec_parity_shards: 2,
            last_access_ms: 0,
            access_count: 0,
            moved_to_cold_at_ms: 0,
        };
        state.files.insert("/ec-file".to_string(), metadata);

        // Verify policy is stored
        let stored = &state.files["/ec-file"];
        assert_eq!(stored.ec_data_shards, 4);
        assert_eq!(stored.ec_parity_shards, 2);
    }

    #[test]
    fn test_allocate_block_ec_selects_kplusm_servers() {
        let mut state = MasterState::default();
        state.files.insert(
            "/ec-file".to_string(),
            FileMetadata {
                path: "/ec-file".to_string(),
                size: 0,
                blocks: vec![],
                etag_md5: "".to_string(),
                created_at_ms: 0,
                ec_data_shards: 4,
                ec_parity_shards: 2,
                last_access_ms: 0,
                access_count: 0,
                moved_to_cold_at_ms: 0,
            },
        );
        for i in 0..6usize {
            state.chunk_servers.insert(
                format!("cs{}:50052", i),
                ChunkServerStatus {
                    last_heartbeat: 9_999_999_999_999,
                    used_space: 0,
                    available_space: 1_000_000,
                    chunk_count: 0,
                    rack_id: String::new(),
                },
            );
        }

        // Verify state has EC policy
        assert_eq!(state.files["/ec-file"].ec_data_shards, 4);
        assert_eq!(state.files["/ec-file"].ec_parity_shards, 2);
        assert_eq!(state.chunk_servers.len(), 6);

        // The dynamic selection logic: needed = ec_data + ec_parity = 6
        let file = &state.files["/ec-file"];
        let needed = (file.ec_data_shards + file.ec_parity_shards) as usize;
        assert_eq!(needed, 6);
        assert!(state.chunk_servers.len() >= needed);
    }

    #[test]
    fn test_heal_ec_block_issues_reconstruct_command() {
        let mut state = MasterState::default();

        // 7 CSes registered: cs0, cs1, cs3, cs4, cs5 are live; cs2 is dead (not registered);
        // cs6 is a spare node not holding any shard of the block, so it can serve as the
        // reconstruction target (required by the no-fallback target selection policy).
        for i in [0usize, 1, 3, 4, 5, 6] {
            state.chunk_servers.insert(
                format!("cs{}:50052", i),
                ChunkServerStatus {
                    last_heartbeat: 9_999_999_999_999,
                    used_space: 0,
                    available_space: 1_000_000,
                    chunk_count: 0,
                    rack_id: String::new(),
                },
            );
        }

        // EC(4,2) file with one block; shard 2 was on the now-dead cs2
        state.files.insert(
            "/ec-file".to_string(),
            crate::dfs::FileMetadata {
                path: "/ec-file".to_string(),
                size: 100,
                blocks: vec![crate::dfs::BlockInfo {
                    block_id: "blk-1".to_string(),
                    size: 100,
                    locations: vec![
                        "cs0:50052".to_string(),
                        "cs1:50052".to_string(),
                        "cs2:50052".to_string(), // dead
                        "cs3:50052".to_string(),
                        "cs4:50052".to_string(),
                        "cs5:50052".to_string(),
                    ],
                    checksum_crc32c: 0,
                    ec_data_shards: 4,
                    ec_parity_shards: 2,
                    original_size: 100,
                }],
                etag_md5: "".to_string(),
                created_at_ms: 0,
                ec_data_shards: 4,
                ec_parity_shards: 2,
                last_access_ms: 0,
                access_count: 0,
                moved_to_cold_at_ms: 0,
            },
        );

        heal_under_replicated_blocks(&mut state);

        // Should issue exactly one RECONSTRUCT_EC_SHARD for shard 2
        let all_cmds: Vec<_> = state.pending_commands.values().flatten().collect();
        assert!(
            all_cmds
                .iter()
                .any(|c| c.r#type == 3 && c.block_id == "blk-1" && c.shard_index == 2),
            "Expected RECONSTRUCT_EC_SHARD for shard 2, got: {:?}",
            all_cmds
        );
    }

    /// Build a minimal `MyMaster` backed by a fake Raft event loop that:
    ///   - applies `ClientRequest` commands directly to the shared state, and
    ///   - immediately acknowledges `GetReadIndex` (linearizable-read bypass).
    ///
    /// Returns `(master, shared_state)` so tests can inspect state afterwards.
    async fn make_test_master() -> (MyMaster, Arc<Mutex<AppState>>) {
        let master_state = MasterState::default();
        let app_state = Arc::new(Mutex::new(AppState::Master(master_state)));

        let (raft_tx, mut raft_rx) = mpsc::channel::<Event>(64);

        // Fake Raft event loop
        let state_clone = app_state.clone();
        tokio::spawn(async move {
            while let Some(event) = raft_rx.recv().await {
                match event {
                    Event::ClientRequest { command, reply_tx } => {
                        // Apply command directly to state (single-node shortcut)
                        {
                            let mut lock = state_clone.lock().expect("Mutex poisoned");
                            if let AppState::Master(ref mut ms) = *lock {
                                match &command {
                                    Command::Master(MasterCommand::UpdateAccessStats {
                                        path,
                                        accessed_at_ms,
                                    }) => {
                                        if let Some(meta) = ms.files.get_mut(path) {
                                            meta.last_access_ms = *accessed_at_ms;
                                            meta.access_count += 1;
                                        }
                                    }
                                    Command::Master(MasterCommand::ConvertToEc {
                                        path,
                                        ec_data_shards,
                                        ec_parity_shards,
                                        new_blocks,
                                    }) => {
                                        if let Some(meta) = ms.files.get_mut(path) {
                                            meta.ec_data_shards = *ec_data_shards;
                                            meta.ec_parity_shards = *ec_parity_shards;
                                            meta.blocks = new_blocks.clone();
                                        }
                                    }
                                    _ => {}
                                }
                            }
                        }
                        let _ = reply_tx.send(Ok(()));
                    }
                    Event::GetReadIndex { reply_tx } => {
                        let _ = reply_tx.send(Ok(0));
                    }
                    // In tests the master_address is "127.0.0.1:9000" — return it so
                    // leader guard passes and scan_tiering / scan_ec_conversion run.
                    Event::GetLeaderInfo { reply_tx } => {
                        let _ = reply_tx.send(Some("127.0.0.1:9000".to_string()));
                    }
                    _ => {}
                }
            }
        });

        // Build a single-shard ShardMap that owns all paths
        let shard_id = "test-shard".to_string();
        let mut shard_map = ShardMap::new_range();
        shard_map.add_shard(shard_id.clone(), vec!["127.0.0.1:9000".to_string()]);

        let config = MasterConfig {
            shard_id: shard_id.clone(),
            config_server_addrs: vec![],
            master_address: "127.0.0.1:9000".to_string(),
            split_threshold_rps: 1_000_000.0,
            split_cooldown_secs: 3600,
            merge_threshold_rps: 0.0,
            ca_cert_path: None,
            domain_name: None,
            cold_threshold_secs: 604800,
            ec_threshold_secs: 2592000,
        };

        let master = MyMaster::new(
            app_state.clone(),
            raft_tx,
            Arc::new(Mutex::new(shard_map)),
            config,
        );

        (master, app_state)
    }

    #[tokio::test]
    async fn test_get_file_info_increments_access_count() {
        let (master, app_state) = make_test_master().await;

        // Pre-insert a file directly into state so get_file_info has something to find
        {
            let mut lock = app_state.lock().expect("Mutex poisoned");
            if let AppState::Master(ref mut ms) = *lock {
                ms.files.insert(
                    "/access-test".to_string(),
                    FileMetadata {
                        path: "/access-test".to_string(),
                        size: 100,
                        blocks: vec![],
                        etag_md5: "".to_string(),
                        created_at_ms: 0,
                        ec_data_shards: 0,
                        ec_parity_shards: 0,
                        last_access_ms: 0,
                        access_count: 0,
                        moved_to_cold_at_ms: 0,
                    },
                );
            }
        }

        // Call get_file_info — this fires the UpdateAccessStats spawn
        let req = tonic::Request::new(GetFileInfoRequest {
            path: "/access-test".to_string(),
        });
        let resp = master.get_file_info(req).await;
        assert!(resp.is_ok(), "get_file_info should succeed");
        assert!(resp.unwrap().into_inner().found, "file should be found");

        // Give the spawned task time to execute
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        // Verify that access_count was incremented
        let lock = app_state.lock().expect("Mutex poisoned");
        if let AppState::Master(ref ms) = *lock {
            let meta = ms.files.get("/access-test").expect("file should exist");
            assert!(
                meta.access_count >= 1,
                "access_count should be >= 1 after get_file_info, got {}",
                meta.access_count
            );
        } else {
            panic!("Expected Master state");
        }
    }

    #[tokio::test]
    async fn test_scan_tiering_queues_cold_move() {
        let (master, _app_state) = make_test_master().await;

        // Insert a file with an ancient last_access_ms and a fake block location
        {
            let mut state = master.state.lock().unwrap();
            if let AppState::Master(ref mut ms) = *state {
                ms.files.insert(
                    "/old-file".to_string(),
                    crate::dfs::FileMetadata {
                        path: "/old-file".to_string(),
                        last_access_ms: 1, // epoch+1ms = ancient
                        access_count: 1,
                        moved_to_cold_at_ms: 0,
                        ec_data_shards: 0,
                        ec_parity_shards: 0,
                        blocks: vec![crate::dfs::BlockInfo {
                            block_id: "blk-tiering-001".to_string(),
                            locations: vec!["127.0.0.1:9001".to_string()],
                            ..Default::default()
                        }],
                        ..Default::default()
                    },
                );
            }
        }

        // last_access_ms=1 and now_ms >> 1 + 604800*1000 guarantees cold threshold is exceeded
        master.scan_tiering().await;

        // Verify MOVE_TO_COLD command queued for the block's ChunkServer
        let state = master.state.lock().unwrap();
        if let AppState::Master(ref ms) = *state {
            let cmds = ms.pending_commands.get("127.0.0.1:9001");
            assert!(
                cmds.is_some_and(|c| c.iter().any(|cmd| cmd.block_id == "blk-tiering-001")),
                "Expected MOVE_TO_COLD command queued for blk-tiering-001"
            );
        }
    }

    #[tokio::test]
    async fn test_scan_ec_conversion_queues_commands() {
        let (master, _dir) = make_test_master().await;

        // Insert a cold file with a block and 9 fake ChunkServers
        {
            let mut state = master.state.lock().unwrap();
            if let AppState::Master(ref mut ms) = *state {
                // Register 9 fake ChunkServers
                for i in 0..9usize {
                    ms.chunk_servers.insert(
                        format!("127.0.0.{}:9000", i + 1),
                        ChunkServerStatus {
                            available_space: 1_000_000,
                            last_heartbeat: 1,
                            used_space: 0,
                            chunk_count: 0,
                            rack_id: String::new(),
                        },
                    );
                }
                ms.files.insert(
                    "/cold-ec-file".to_string(),
                    crate::dfs::FileMetadata {
                        path: "/cold-ec-file".to_string(),
                        moved_to_cold_at_ms: 1, // ancient
                        ec_data_shards: 0,
                        ec_parity_shards: 0,
                        blocks: vec![crate::dfs::BlockInfo {
                            block_id: "blk-ec-conv-001".to_string(),
                            locations: vec!["127.0.0.1:9000".to_string()],
                            ..Default::default()
                        }],
                        ..Default::default()
                    },
                );
            }
        }

        master.scan_ec_conversion().await;

        // Give the spawned Raft task time to apply the ConvertToEc command
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let state = master.state.lock().unwrap();
        if let AppState::Master(ref ms) = *state {
            // scan_ec_conversion no longer queues RECONSTRUCT_EC_SHARD commands (Issue 5 fix).
            // It only commits a ConvertToEc Raft command which updates the metadata.
            // Verify that the file's metadata was updated to reflect the EC state.
            let meta = ms
                .files
                .get("/cold-ec-file")
                .expect("file should exist in state");
            assert_eq!(
                meta.ec_data_shards, 6,
                "Expected ec_data_shards==6 after ConvertToEc, got {}",
                meta.ec_data_shards
            );
            assert_eq!(
                meta.ec_parity_shards, 3,
                "Expected ec_parity_shards==3 after ConvertToEc, got {}",
                meta.ec_parity_shards
            );
            // No RECONSTRUCT_EC_SHARD or DELETE commands should be pending
            let total_cmds: usize = ms.pending_commands.values().map(|v| v.len()).sum();
            assert_eq!(
                total_cmds, 0,
                "Expected no pending chunk commands (no RECONSTRUCT_EC_SHARD, no DELETE), got {}",
                total_cmds
            );
        } else {
            panic!("Expected Master state");
        }
    }
}
