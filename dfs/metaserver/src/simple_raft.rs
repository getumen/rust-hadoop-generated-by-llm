//! # Simple Raft Consensus Implementation
//!
//! This module implements the Raft consensus algorithm for distributed state machine
//! replication in the DFS Master server.
//!
//! ## Overview
//!
//! The implementation follows the Raft paper (Ongaro & Ousterhout, 2014) and includes:
//!
//! - **Leader Election**: Randomized election timeouts with majority voting
//! - **Log Replication**: Append-only log with strong consistency guarantees
//! - **Persistence**: RocksDB-backed storage for term, vote, and log entries
//! - **Snapshotting**: Automatic log compaction with state machine snapshots
//! - **Membership Changes**: Single-server configuration changes
//! - **ReadIndex**: Linearizable reads without log replication
//!
//! ## Architecture
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────────┐
//! │                         RaftNode                                │
//! │  ┌──────────┐  ┌──────────┐  ┌──────────┐  ┌──────────────────┐ │
//! │  │  State   │  │   Log    │  │ RocksDB  │  │   HTTP Client    │ │
//! │  │ Machine  │  │ Entries  │  │ Persist  │  │ (Peer Comm)      │ │
//! │  └──────────┘  └──────────┘  └──────────┘  └──────────────────┘ │
//! └─────────────────────────────────────────────────────────────────┘
//! ```
//!
//! ## Key Types
//!
//! - [`RaftNode`]: The main Raft node implementation
//! - [`Role`]: Node role (Follower, Candidate, Leader)
//! - [`Command`]: Application commands to be replicated
//! - [`Event`]: Events processed by the Raft event loop
//! - [`RpcMessage`]: Raft RPC messages (RequestVote, AppendEntries, etc.)

use crate::dfs::FileMetadata;
use crate::master::MasterState;
use anyhow::{Context, Result};
use dfs_common::sharding::ShardMap;
use rand::Rng;
use rocksdb::DB;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;
use tokio::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Role {
    Follower,
    Candidate,
    Leader,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogEntry {
    pub term: u64,
    pub command: Command,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Command {
    Master(MasterCommand),
    Config(ConfigCommand),
    Membership(MembershipCommand),
    NoOp,
}

/// Commands for Raft cluster membership changes
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum MembershipCommand {
    /// Add a new server to the Raft cluster (legacy single-phase)
    AddServer {
        /// Server ID (typically the node index)
        server_id: usize,
        /// Server address (HTTP endpoint for Raft RPC)
        server_address: String,
    },
    /// Remove a server from the Raft cluster (legacy single-phase)
    RemoveServer {
        /// Server ID to remove
        server_id: usize,
    },
    /// Add multiple servers to the cluster (new multi-server support)
    AddServers {
        /// Servers to add (server_id -> address)
        servers: HashMap<usize, String>,
    },
    /// Remove multiple servers from the cluster (new multi-server support)
    RemoveServers {
        /// Server IDs to remove
        server_ids: Vec<usize>,
    },
    /// Internal: Transition from C-old to C-old,new (joint consensus)
    BeginJointConsensus {
        old_members: HashMap<usize, String>,
        new_members: HashMap<usize, String>,
        version: u64,
    },
    /// Internal: Transition from C-old,new to C-new (finalize)
    FinalizeConfiguration {
        new_members: HashMap<usize, String>,
        version: u64,
    },
}

/// Represents the state of a cluster configuration
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum ClusterConfiguration {
    /// Simple configuration with a single member list
    Simple {
        /// All voting members (server_id -> address)
        members: HashMap<usize, String>,
        /// Configuration version (monotonically increasing)
        version: u64,
    },
    /// Joint configuration during membership transition
    Joint {
        /// Old configuration members
        old_members: HashMap<usize, String>,
        /// New configuration members
        new_members: HashMap<usize, String>,
        /// Configuration version
        version: u64,
    },
}

impl ClusterConfiguration {
    /// Get all voting members (union of old and new in joint config)
    pub fn all_members(&self) -> HashMap<usize, String> {
        match self {
            ClusterConfiguration::Simple { members, .. } => members.clone(),
            ClusterConfiguration::Joint {
                old_members,
                new_members,
                ..
            } => {
                let mut all = old_members.clone();
                all.extend(new_members.clone());
                all
            }
        }
    }

    /// Check if a majority is achieved in both configurations (for joint consensus)
    pub fn has_joint_majority(&self, acks: &HashSet<usize>) -> bool {
        match self {
            ClusterConfiguration::Simple { members, .. } => {
                let member_count = members.len();
                let ack_count = acks.iter().filter(|id| members.contains_key(id)).count();
                ack_count > member_count / 2
            }
            ClusterConfiguration::Joint {
                old_members,
                new_members,
                ..
            } => {
                let old_count = old_members.len();
                let new_count = new_members.len();
                let old_acks = acks
                    .iter()
                    .filter(|id| old_members.contains_key(id))
                    .count();
                let new_acks = acks
                    .iter()
                    .filter(|id| new_members.contains_key(id))
                    .count();
                old_acks > old_count / 2 && new_acks > new_count / 2
            }
        }
    }

    /// Get configuration version
    pub fn version(&self) -> u64 {
        match self {
            ClusterConfiguration::Simple { version, .. } => *version,
            ClusterConfiguration::Joint { version, .. } => *version,
        }
    }
}

/// Server role in the cluster
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ServerRole {
    /// Voting member (participates in elections and quorum)
    Voting,
    /// Non-voting member (receives log replication but doesn't vote)
    NonVoting,
}

/// Tracks the state of an ongoing configuration change
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ConfigChangeState {
    /// No configuration change in progress
    None,
    /// Adding new servers (in catch-up phase)
    AddingServers {
        /// Servers being added (server_id -> (address, catch_up_progress))
        servers: HashMap<usize, (String, CatchUpProgress)>,
        /// When the change started
        started_at: u64,
    },
    /// In joint consensus (C-old,new)
    InJointConsensus {
        /// The joint configuration log index
        joint_config_index: usize,
        /// Target configuration after joint phase
        target_config: HashMap<usize, String>,
    },
    /// Waiting for leader transfer before removal
    TransferringLeadership {
        /// Target server to transfer to
        target_server: usize,
        /// Servers to remove after transfer
        servers_to_remove: Vec<usize>,
    },
}

/// Tracks catch-up progress for a non-voting member
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CatchUpProgress {
    /// Match index for this server
    pub match_index: usize,
    /// Number of successful replication rounds
    pub rounds_caught_up: usize,
    /// When this server was added
    pub added_at: u64,
}

impl CatchUpProgress {
    pub fn new(current_time: u64) -> Self {
        Self {
            match_index: 0,
            rounds_caught_up: 0,
            added_at: current_time,
        }
    }

    /// Check if server is caught up (needs 10 rounds of successful replication)
    pub fn is_caught_up(&self, leader_commit_index: usize) -> bool {
        self.match_index >= leader_commit_index && self.rounds_caught_up >= 10
    }

    /// Update progress after successful replication
    pub fn update(&mut self, new_match_index: usize) {
        if new_match_index > self.match_index {
            self.match_index = new_match_index;
            self.rounds_caught_up += 1;
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum MasterCommand {
    CreateFile {
        path: String,
    },
    DeleteFile {
        path: String,
    },

    AllocateBlock {
        path: String,
        block_id: String,
        locations: Vec<String>,
    },
    RegisterChunkServer {
        address: String,
    },
    // =========================================================================
    // Transaction Record Commands for Cross-Shard Operations
    // =========================================================================
    /// Same-shard rename operation (no transaction record needed)
    RenameFile {
        source_path: String,
        dest_path: String,
    },
    /// Create a new transaction record (state = Pending or Prepared)
    CreateTransactionRecord {
        record: crate::master::TransactionRecord,
    },
    /// Update transaction state (Pending -> Prepared -> Committed/Aborted)
    UpdateTransactionState {
        tx_id: String,
        new_state: crate::master::TxState,
    },
    /// Apply transaction operation (create/delete file as part of transaction)
    ApplyTransactionOperation {
        tx_id: String,
        operation: crate::master::TxOperation,
    },
    /// Delete transaction record (for garbage collection)
    DeleteTransactionRecord {
        tx_id: String,
    },
    SplitShard {
        split_key: String,
        new_shard_id: String,
        new_shard_peers: Vec<String>,
    },
    MergeShard {
        victim_shard_id: String,
    },
    /// Ingest a batch of file metadata (for shard transfers)
    IngestBatch {
        files: Vec<FileMetadata>,
    },
    /// Trigger background data shuffling for a prefix
    TriggerShuffle {
        prefix: String,
    },
    /// Finalize file (update size)
    CompleteFile {
        path: String,
        size: u64,
    },
    /// Stop background data shuffling for a prefix
    StopShuffle {
        prefix: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ConfigCommand {
    AddShard {
        shard_id: String,
        peers: Vec<String>,
    },
    RemoveShard {
        shard_id: String,
    },
    SplitShard {
        shard_id: String,
        split_key: String,
        new_shard_id: String,
        new_shard_peers: Vec<String>,
    },
    MergeShard {
        victim_shard_id: String,
        retained_shard_id: String,
    },
    RebalanceShard {
        old_key: String,
        new_key: String,
    },
    RegisterMaster {
        address: String,
        shard_id: String,
    },
    ShardHeartbeat {
        address: String,
        rps_per_prefix: HashMap<String, f64>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MasterInfo {
    pub address: String,
    pub shard_id: String,
    pub last_heartbeat: u64,
    pub rps_per_prefix: HashMap<String, f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigStateInner {
    pub shard_map: ShardMap,
    pub masters: HashMap<String, MasterInfo>,
}

#[derive(Debug, Serialize, Deserialize)]
pub enum AppState {
    Master(MasterState),
    Config(ConfigStateInner),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    pub last_included_index: usize,
    pub last_included_term: u64,
    pub data: Vec<u8>, // Serialized MasterState
}

/// RPC to trigger immediate leader transfer
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TimeoutNowArgs {
    pub term: u64,
    pub sender_id: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TimeoutNowReply {
    pub term: u64,
    pub success: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum RpcMessage {
    RequestVote(RequestVoteArgs),
    RequestVoteResponse(RequestVoteReply),
    AppendEntries(AppendEntriesArgs),
    AppendEntriesResponse(AppendEntriesReply),
    InstallSnapshot(InstallSnapshotArgs),
    InstallSnapshotResponse(InstallSnapshotReply),
    TimeoutNow(TimeoutNowArgs),
    TimeoutNowResponse(TimeoutNowReply),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequestVoteArgs {
    pub term: u64,
    pub candidate_id: usize,
    pub last_log_index: usize,
    pub last_log_term: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequestVoteReply {
    pub term: u64,
    pub vote_granted: bool,
    pub peer_id: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppendEntriesArgs {
    pub term: u64,
    pub leader_id: usize,
    pub prev_log_index: usize,
    pub prev_log_term: u64,
    pub entries: Vec<LogEntry>,
    pub leader_commit: usize,
    pub leader_address: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppendEntriesReply {
    pub term: u64,
    pub success: bool,
    pub match_index: usize,
    pub peer_id: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstallSnapshotArgs {
    pub term: u64,
    pub leader_id: usize,
    pub last_included_index: usize,
    pub last_included_term: u64,
    pub data: Vec<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstallSnapshotReply {
    pub term: u64,
    pub last_included_index: usize,
    pub peer_id: usize,
}

pub struct ReadIndexRequest {
    pub read_index: usize,
    pub term: u64,
    pub acks: HashSet<usize>,
    pub majority_confirmed: bool,
    pub reply_tx: tokio::sync::oneshot::Sender<Result<usize, Option<String>>>,
}

pub enum Event {
    Rpc {
        msg: RpcMessage,
        reply_tx: Option<tokio::sync::oneshot::Sender<RpcMessage>>,
    },
    ClientRequest {
        command: Command,
        reply_tx: tokio::sync::oneshot::Sender<Result<(), Option<String>>>,
    },
    GetLeaderInfo {
        reply_tx: tokio::sync::oneshot::Sender<Option<String>>,
    },
    GetClusterInfo {
        reply_tx: tokio::sync::oneshot::Sender<ClusterInfo>,
    },
    GetReadIndex {
        reply_tx: tokio::sync::oneshot::Sender<Result<usize, Option<String>>>,
    },
}

/// Information about the Raft cluster state
#[derive(Debug, Clone, Serialize)]
pub struct ClusterInfo {
    pub node_id: usize,
    pub role: Role,
    pub current_term: u64,
    pub leader_id: Option<usize>,
    pub leader_address: Option<String>,
    pub peers: Vec<String>,
    pub commit_index: usize,
    pub last_applied: usize,
    pub log_len: usize,
    pub votes_received: usize,
    pub cluster_config: ClusterConfiguration,
    pub config_change_state: ConfigChangeState,
}

/// A single Raft consensus node.
///
/// The `RaftNode` manages all Raft state including:
/// - Persistent state: current term, voted_for, log entries (backed by RocksDB)
/// - Volatile state: commit_index, last_applied, role
/// - Leader state: next_index and match_index for each follower
///
/// # Event Loop
///
/// The node runs an async event loop that:
/// 1. Handles incoming RPC messages from peers
/// 2. Processes client requests (log append)
/// 3. Triggers elections on timeout (followers/candidates)
/// 4. Sends heartbeats (leaders)
/// 5. Applies committed log entries to the state machine
///
/// # Thread Safety
///
/// The node itself is single-threaded (owned by one tokio task).
/// The `app_state` is wrapped in `Arc<Mutex<>>` for cross-task access.
pub struct RaftNode {
    /// Unique identifier for this node in the cluster (0-indexed).
    pub id: usize,
    /// HTTP addresses of peer nodes for RPC communication.
    pub peers: Vec<String>,
    /// Address advertised to clients for redirection hints.
    pub client_address: String,
    /// Current role in the Raft cluster.
    pub role: Role,
    /// Latest term this server has seen (persisted to disk).
    pub current_term: u64,
    /// Candidate ID that received our vote in current term (persisted).
    pub voted_for: Option<usize>,
    /// Log entries; first entry is dummy (index 0 = last_included_index).
    pub log: Vec<LogEntry>,
    /// Index of highest log entry known to be committed.
    pub commit_index: usize,
    /// Index of highest log entry applied to state machine.
    pub last_applied: usize,
    /// For each peer: index of next log entry to send (leader only).
    pub next_index: Vec<usize>,
    /// For each peer: index of highest log entry known to be replicated (leader only).
    pub match_index: Vec<usize>,
    /// ID of the current leader (if known).
    pub current_leader: Option<usize>,
    /// Address of the current leader (for client redirection).
    pub current_leader_address: Option<String>,

    // Snapshot metadata
    /// Index of the last log entry included in the most recent snapshot.
    pub last_included_index: usize,
    /// Term of the last log entry included in the most recent snapshot.
    pub last_included_term: u64,

    /// Randomized election timeout (typically 150-300ms range, extended here).
    pub election_timeout: Duration,
    /// Timestamp of the last election timeout reset.
    pub last_election_time: Instant,

    /// Channel receiver for incoming events (RPCs, client requests).
    pub inbox: mpsc::Receiver<Event>,
    /// Self-sender for queuing internal events (e.g., RPC responses).
    pub self_tx: mpsc::Sender<Event>,
    /// Shared application state (the actual state machine being replicated).
    pub app_state: Arc<Mutex<AppState>>,
    /// Number of votes received in the current election (candidate only).
    pub votes_received: usize,
    /// IDs of servers that voted for us in current election (for joint consensus).
    pub voters: HashSet<usize>,

    /// RocksDB instance for persistent storage.
    pub db: Arc<DB>,
    /// HTTP client for sending RPCs to peers.
    pub http_client: reqwest::Client,

    /// Pending ReadIndex requests awaiting majority confirmation.
    pub pending_read_indices: Vec<ReadIndexRequest>,
    /// Timestamp of the last heartbeat sent (leader only).
    pub last_heartbeat_time: Instant,
    /// Timestamp of the last majority acknowledgment received.
    pub last_majority_ack_time: Instant,

    // Dynamic membership change fields
    /// Current cluster configuration (Simple or Joint consensus).
    pub cluster_config: ClusterConfiguration,
    /// Server roles (voting vs non-voting).
    pub server_roles: HashMap<usize, ServerRole>,
    /// Current configuration change state.
    pub config_change_state: ConfigChangeState,
    /// Non-voting members being added (for catch-up).
    pub non_voting_members: HashMap<usize, String>,
    /// Catch-up progress tracking for non-voting members.
    pub catch_up_progress: HashMap<usize, CatchUpProgress>,
    /// Last committed configuration index (to prevent concurrent changes).
    pub last_committed_config_index: usize,
    /// Monotonic time counter (for tracking timeouts and progress).
    pub monotonic_time: u64,
}

impl RaftNode {
    pub fn new(
        id: usize,
        peers: Vec<String>,
        client_address: String,
        storage_dir: String,
        app_state: Arc<Mutex<AppState>>,
        inbox: mpsc::Receiver<Event>,
        self_tx: mpsc::Sender<Event>,
    ) -> Self {
        let path = format!("{}/raft_node_{}", storage_dir, id);
        let db = DB::open_default(&path).expect("Failed to open RocksDB");
        let db = Arc::new(db);

        let (current_term, voted_for, log, last_included_index, last_included_term) =
            Self::load_state(&db, &app_state);

        // Note: 500ms timeout may be aggressive for large snapshot transfers.
        // Consider increasing timeout for InstallSnapshot RPCs if needed.
        let http_client = reqwest::Client::builder()
            .timeout(Duration::from_millis(500))
            .build()
            .expect("Failed to build HTTP client");

        // Initialize cluster configuration from peers
        let mut initial_members = HashMap::new();
        for (idx, peer) in peers.iter().enumerate() {
            initial_members.insert(idx, peer.clone());
        }
        initial_members.insert(id, client_address.clone());

        // Load or create cluster configuration
        let (loaded_config, config_change_state) = Self::load_config(&db);
        let cluster_config = match &loaded_config {
            ClusterConfiguration::Simple { members, .. } if members.is_empty() => {
                // First time initialization
                ClusterConfiguration::Simple {
                    members: initial_members.clone(),
                    version: 0,
                }
            }
            _ => loaded_config,
        };

        let mut server_roles = HashMap::new();
        for member_id in cluster_config.all_members().keys() {
            server_roles.insert(*member_id, ServerRole::Voting);
        }

        RaftNode {
            id,
            peers,
            client_address,
            role: Role::Follower,
            current_term,
            voted_for,
            log,
            commit_index: last_included_index,
            last_applied: last_included_index,
            next_index: vec![],
            match_index: vec![],
            current_leader: None,
            current_leader_address: None,
            last_included_index,
            last_included_term,
            election_timeout: Duration::from_millis(rand::thread_rng().gen_range(1500..3000)),
            last_election_time: Instant::now(),
            inbox,
            self_tx,
            app_state,
            votes_received: 0,
            voters: HashSet::new(),
            db,
            http_client,
            pending_read_indices: vec![],
            last_heartbeat_time: Instant::now(),
            last_majority_ack_time: Instant::now(),
            cluster_config,
            server_roles,
            config_change_state,
            non_voting_members: HashMap::new(),
            catch_up_progress: HashMap::new(),
            last_committed_config_index: 0,
            monotonic_time: 0,
        }
    }

    fn load_state(
        db: &DB,
        app_state: &Arc<Mutex<AppState>>,
    ) -> (u64, Option<usize>, Vec<LogEntry>, usize, u64) {
        let term = match db.get(b"term") {
            Ok(Some(val)) => match val.try_into() {
                Ok(bytes) => u64::from_be_bytes(bytes),
                Err(_) => {
                    tracing::error!("Error: Corrupted term data in DB");
                    0
                }
            },
            _ => 0,
        };
        let voted_for = match db.get(b"vote") {
            Ok(Some(val)) => match val.try_into() {
                Ok(bytes) => Some(usize::from_be_bytes(bytes)),
                Err(_) => {
                    tracing::error!("Error: Corrupted vote data in DB");
                    None
                }
            },
            _ => None,
        };

        // Load snapshot if exists
        let (last_included_index, last_included_term) = match db.get(b"snapshot_meta") {
            Ok(Some(val)) => {
                match serde_json::from_slice::<(usize, u64)>(&val) {
                    Ok(meta) => {
                        // Load snapshot data and restore app state
                        if let Ok(Some(snapshot_data)) = db.get(b"snapshot_data") {
                            match serde_json::from_slice::<AppState>(&snapshot_data) {
                                Ok(state) => {
                                    *app_state.lock().expect("Mutex poisoned") = state;
                                }
                                Err(_) => {
                                    // Try legacy MasterState
                                    match serde_json::from_slice::<MasterState>(&snapshot_data) {
                                        Ok(state) => {
                                            *app_state.lock().expect("Mutex poisoned") =
                                                AppState::Master(state);
                                        }
                                        Err(e) => {
                                            tracing::error!(
                                                "Error: Failed to deserialize snapshot data: {}",
                                                e
                                            );
                                        }
                                    }
                                }
                            }
                        }
                        meta
                    }
                    Err(e) => {
                        tracing::error!("Error: Failed to deserialize snapshot metadata: {}", e);
                        (0, 0)
                    }
                }
            }
            _ => (0, 0),
        };

        let mut log = vec![LogEntry {
            term: last_included_term,
            command: Command::NoOp,
        }];
        let mut index = last_included_index + 1;
        loop {
            let key = format!("log:{}", index);
            match db.get(key.as_bytes()) {
                Ok(Some(val)) => match serde_json::from_slice::<LogEntry>(&val) {
                    Ok(entry) => {
                        log.push(entry);
                        index += 1;
                    }
                    Err(e) => {
                        tracing::error!(
                            "Error: Failed to deserialize log entry at index {}: {}",
                            index,
                            e
                        );
                        break;
                    }
                },
                _ => break,
            }
        }

        (
            term,
            voted_for,
            log,
            last_included_index,
            last_included_term,
        )
    }

    fn save_term(&self) -> Result<()> {
        self.db
            .put(b"term", self.current_term.to_be_bytes())
            .context("Failed to save term to DB")?;
        Ok(())
    }

    fn save_vote(&self) -> Result<()> {
        if let Some(vote) = self.voted_for {
            self.db
                .put(b"vote", vote.to_be_bytes())
                .context("Failed to save vote to DB")?;
        } else {
            self.db
                .delete(b"vote")
                .context("Failed to delete vote from DB")?;
        }
        Ok(())
    }

    fn save_log_entry(&self, index: usize, entry: &LogEntry) -> Result<()> {
        let key = format!("log:{}", index);
        let val = serde_json::to_vec(entry).context("Failed to serialize log entry")?;
        self.db
            .put(key.as_bytes(), val)
            .context("Failed to save log entry to DB")?;
        Ok(())
    }

    fn save_config(&self) -> Result<()> {
        let serialized =
            serde_json::to_vec(&self.cluster_config).context("Failed to serialize config")?;
        self.db
            .put(b"cluster_config", serialized)
            .context("Failed to save cluster config")?;

        let state_serialized = serde_json::to_vec(&self.config_change_state)
            .context("Failed to serialize config change state")?;
        self.db
            .put(b"config_change_state", state_serialized)
            .context("Failed to save config change state")?;

        Ok(())
    }

    fn load_config(db: &DB) -> (ClusterConfiguration, ConfigChangeState) {
        let config = match db.get(b"cluster_config") {
            Ok(Some(val)) => serde_json::from_slice(&val).unwrap_or_else(|e| {
                tracing::warn!("Failed to deserialize cluster config: {}", e);
                ClusterConfiguration::Simple {
                    members: HashMap::new(),
                    version: 0,
                }
            }),
            _ => ClusterConfiguration::Simple {
                members: HashMap::new(),
                version: 0,
            },
        };

        let state = match db.get(b"config_change_state") {
            Ok(Some(val)) => serde_json::from_slice(&val).unwrap_or(ConfigChangeState::None),
            _ => ConfigChangeState::None,
        };

        (config, state)
    }

    fn check_read_indices(&mut self) {
        let mut satisfied_indices = Vec::new();
        for (idx, req) in self.pending_read_indices.iter().enumerate() {
            if req.majority_confirmed && self.last_applied >= req.read_index {
                satisfied_indices.push(idx);
            }
        }

        for idx in satisfied_indices.into_iter().rev() {
            let req = self.pending_read_indices.remove(idx);
            tracing::info!(
                "Node {} (L) satisfying ReadIndex request with read_index {} (last_applied: {})",
                self.id,
                req.read_index,
                self.last_applied
            );
            let _ = req.reply_tx.send(Ok(req.read_index));
        }
    }

    fn delete_log_entries_from(&self, start_index: usize) -> Result<()> {
        let mut index = start_index;
        loop {
            let key = format!("log:{}", index);
            if self
                .db
                .get(key.as_bytes())
                .context("Failed to read from DB")?
                .is_none()
            {
                break;
            }
            self.db
                .delete(key.as_bytes())
                .context("Failed to delete from DB")?;
            index += 1;
        }
        Ok(())
    }

    fn create_snapshot(&mut self) -> Result<()> {
        // Serialize current app state
        let state = self
            .app_state
            .lock()
            .map_err(|_| anyhow::anyhow!("Mutex poisoned"))?;
        let snapshot_data = serde_json::to_vec(&*state).context("Failed to serialize app state")?;
        drop(state);

        // Save snapshot metadata
        let term = if self.last_applied >= self.last_included_index {
            let index = self.last_applied - self.last_included_index;
            self.log
                .get(index)
                .map(|e| e.term)
                .unwrap_or(self.last_included_term)
        } else {
            tracing::error!(
                "Warning: last_applied {} < last_included_index {} during snapshot creation",
                self.last_applied,
                self.last_included_index
            );
            self.last_included_term
        };
        let meta = (self.last_applied, term);
        let meta_bytes =
            serde_json::to_vec(&meta).context("Failed to serialize snapshot metadata")?;
        self.db
            .put(b"snapshot_meta", meta_bytes)
            .context("Failed to save snapshot metadata to DB")?;

        // Save snapshot data
        self.db
            .put(b"snapshot_data", snapshot_data)
            .context("Failed to save snapshot data to DB")?;

        // Delete old log entries
        for i in (self.last_included_index + 1)..=self.last_applied {
            let key = format!("log:{}", i);
            self.db
                .delete(key.as_bytes())
                .context("Failed to delete old log entry from DB")?;
        }

        // Update snapshot metadata
        self.last_included_term = term;

        // Truncate log, keeping only entries after snapshot
        let new_log_start = self.last_applied - self.last_included_index + 1;
        let new_log = self.log[new_log_start..].to_vec();
        self.log = vec![LogEntry {
            term: self.last_included_term,
            command: Command::NoOp,
        }];
        self.log.extend(new_log);

        self.last_included_index = self.last_applied;

        tracing::info!(
            "Node {} created snapshot at index {}",
            self.id,
            self.last_included_index
        );
        Ok(())
    }

    fn install_snapshot(&mut self, snapshot: Snapshot) -> Result<()> {
        // Save snapshot to disk
        let meta = (snapshot.last_included_index, snapshot.last_included_term);
        let meta_bytes =
            serde_json::to_vec(&meta).context("Failed to serialize snapshot metadata")?;
        self.db
            .put(b"snapshot_meta", meta_bytes)
            .context("Failed to save snapshot metadata to DB")?;
        self.db
            .put(b"snapshot_data", &snapshot.data)
            .context("Failed to save snapshot data to DB")?;

        // Restore app state
        match serde_json::from_slice::<AppState>(&snapshot.data) {
            Ok(state) => {
                *self
                    .app_state
                    .lock()
                    .map_err(|_| anyhow::anyhow!("Mutex poisoned"))? = state
            }
            Err(_e) => {
                // Try legacy MasterState
                match serde_json::from_slice::<MasterState>(&snapshot.data) {
                    Ok(state) => {
                        *self
                            .app_state
                            .lock()
                            .map_err(|_| anyhow::anyhow!("Mutex poisoned"))? =
                            AppState::Master(state)
                    }
                    Err(e) => tracing::error!("Error: Failed to deserialize snapshot data: {}", e),
                }
            }
        }

        // Delete old log entries
        for i in (self.last_included_index + 1)..=snapshot.last_included_index {
            let key = format!("log:{}", i);
            self.db
                .delete(key.as_bytes())
                .context("Failed to delete old log entry from DB")?;
        }

        // Update state
        self.last_included_index = snapshot.last_included_index;
        self.last_included_term = snapshot.last_included_term;
        self.log = vec![LogEntry {
            term: self.last_included_term,
            command: Command::NoOp,
        }];
        self.commit_index = snapshot.last_included_index;
        self.last_applied = snapshot.last_included_index;

        tracing::info!(
            "Node {} installed snapshot at index {}",
            self.id,
            self.last_included_index
        );
        Ok(())
    }

    pub async fn run(&mut self) {
        let tick_rate = Duration::from_millis(100);
        let mut tick_interval = tokio::time::interval(tick_rate);

        self.next_index = vec![self.log.len() + self.last_included_index; self.peers.len()];
        self.match_index = vec![self.last_included_index; self.peers.len()];

        loop {
            tokio::select! {
                _ = tick_interval.tick() => {
                    if let Err(e) = self.tick().await {
                         tracing::error!("Error in tick: {:?}", e);
                    }
                }
                Some(event) = self.inbox.recv() => {
                    if let Err(e) = self.handle_event(event).await {
                        tracing::error!("Error in handle_event: {:?}", e);
                    }
                }
            }
        }
    }

    async fn tick(&mut self) -> Result<()> {
        // Increment monotonic time counter
        self.monotonic_time += 1;

        match self.role {
            Role::Follower | Role::Candidate => {
                if self.last_election_time.elapsed() > self.election_timeout {
                    self.start_election().await?;
                }
            }
            Role::Leader => {
                self.send_heartbeats().await?;

                // Check membership change progress
                self.check_promote_non_voting_members()?;
                self.check_finalize_joint_consensus()?;
            }
        }
        self.apply_logs();

        // Create snapshot if log is too large (threshold: 100 entries)
        const SNAPSHOT_THRESHOLD: usize = 100;
        if self.log.len() > SNAPSHOT_THRESHOLD && self.last_applied > self.last_included_index {
            self.create_snapshot()?;
        }
        Ok(())
    }

    async fn start_election(&mut self) -> Result<()> {
        self.role = Role::Candidate;
        self.current_term += 1;
        self.save_term()?; // Persist term

        self.voted_for = Some(self.id);
        self.save_vote()?; // Persist vote

        self.votes_received = 1;
        self.voters = HashSet::new();
        self.voters.insert(self.id); // Vote for self
        self.last_election_time = Instant::now();
        self.election_timeout = Duration::from_millis(rand::thread_rng().gen_range(1500..3000));

        tracing::info!(
            "Node {} starting election for term {}",
            self.id,
            self.current_term
        );

        let last_log_index = self.log.len() - 1 + self.last_included_index;
        let last_log_term = self.log[self.log.len() - 1].term;

        let args = RequestVoteArgs {
            term: self.current_term,
            candidate_id: self.id,
            last_log_index,
            last_log_term,
        };

        // Check if this is a single-node cluster
        let all_members = self.cluster_config.all_members();
        if all_members.len() == 1 {
            self.become_leader();
            return Ok(());
        }

        for peer in self.peers.iter() {
            let url = format!("{}/raft/vote", peer);
            let args = args.clone();
            let tx = self.self_tx.clone();
            let client = self.http_client.clone();

            tokio::spawn(async move {
                let mut attempt = 0;
                let max_retries = 3;
                let mut delay = Duration::from_millis(50);

                loop {
                    match client.post(&url).json(&args).send().await {
                        Ok(resp) => match resp.json::<RequestVoteReply>().await {
                            Ok(reply) => {
                                let _ = tx
                                    .send(Event::Rpc {
                                        msg: RpcMessage::RequestVoteResponse(reply),
                                        reply_tx: None,
                                    })
                                    .await;
                                break;
                            }
                            Err(e) => {
                                tracing::error!(
                                    "Failed to parse RequestVoteResponse from {}: {}",
                                    url,
                                    e
                                );
                            }
                        },
                        Err(e) => {
                            tracing::warn!(
                                "RequestVote failed to {} (attempt {}/{}): {}",
                                url,
                                attempt + 1,
                                max_retries,
                                e
                            );
                        }
                    }

                    attempt += 1;
                    if attempt >= max_retries {
                        break;
                    }
                    tokio::time::sleep(delay).await;
                    delay *= 2; // Exponential backoff
                }
            });
        }
        Ok(())
    }

    fn become_leader(&mut self) {
        tracing::info!(
            "Node {} became Leader for term {}",
            self.id,
            self.current_term
        );
        self.role = Role::Leader;
        self.current_leader = Some(self.id);
        self.current_leader_address = Some(self.client_address.clone());
        self.next_index = vec![self.log.len() + self.last_included_index; self.peers.len()];
        self.match_index = vec![self.last_included_index; self.peers.len()];

        // For multi-node cluster, we append a NoOp entry to commit entries from previous terms.
        // This is required for ReadIndex safety.
        let entry = LogEntry {
            term: self.current_term,
            command: Command::NoOp,
        };
        self.log.push(entry.clone());
        let absolute_index = self.log.len() - 1 + self.last_included_index;
        if let Err(e) = self.save_log_entry(absolute_index, &entry) {
            tracing::error!("Failed to save log entry in become_leader: {:?}", e);
        }
        // For single-node clusters, immediately commit everything in the log upon becoming leader.
        if self.peers.is_empty() && absolute_index > self.commit_index {
            tracing::info!(
                "Single-node cluster: advancing commit_index to {} on leadership",
                absolute_index
            );
            self.commit_index = absolute_index;
            self.apply_logs();
        }
    }

    async fn send_heartbeats(&mut self) -> Result<()> {
        // Combine voting members (peers) and non-voting members for replication
        let all_members = self.cluster_config.all_members();
        let mut combined_peers: Vec<(usize, String, bool)> = vec![];

        // Add voting members
        for (server_id, addr) in all_members.iter() {
            if *server_id != self.id {
                combined_peers.push((*server_id, addr.clone(), true)); // true = voting
            }
        }

        // Add non-voting members
        for (server_id, addr) in self.non_voting_members.iter() {
            combined_peers.push((*server_id, addr.clone(), false)); // false = non-voting
        }

        for (peer_idx, (_server_id, peer, is_voting)) in combined_peers.iter().enumerate() {
            // For non-voting members, use a separate index tracking
            let i = if *is_voting {
                // Find index in self.peers
                self.peers
                    .iter()
                    .position(|p| p == peer)
                    .unwrap_or(peer_idx)
            } else {
                // For non-voting, we'll extend next_index/match_index temporarily
                // or use default values
                if peer_idx < self.next_index.len() {
                    peer_idx
                } else {
                    continue; // Skip if we don't have tracking arrays set up yet
                }
            };

            if i >= self.next_index.len() {
                continue; // Safety check
            }
            // Check if follower needs a snapshot
            if self.next_index[i] <= self.last_included_index {
                // Send snapshot instead of AppendEntries
                let state = match self.app_state.lock() {
                    Ok(s) => s,
                    Err(e) => {
                        tracing::error!("Failed to lock app_state for snapshot: {:?}", e);
                        continue;
                    }
                };
                let snapshot_data = match serde_json::to_vec(&*state) {
                    Ok(data) => data,
                    Err(e) => {
                        tracing::error!("Failed to serialize app state for snapshot: {:?}", e);
                        continue;
                    }
                };
                drop(state);

                let args = InstallSnapshotArgs {
                    term: self.current_term,
                    leader_id: self.id,
                    last_included_index: self.last_included_index,
                    last_included_term: self.last_included_term,
                    data: snapshot_data,
                };

                let url = format!("{}/raft/snapshot", peer);
                let tx = self.self_tx.clone();
                let client = self.http_client.clone();

                tokio::spawn(async move {
                    let mut attempt = 0;
                    let max_retries = 3;
                    let mut delay = Duration::from_millis(50);

                    loop {
                        match client.post(&url).json(&args).send().await {
                            Ok(resp) => match resp.json::<InstallSnapshotReply>().await {
                                Ok(reply) => {
                                    let _ = tx
                                        .send(Event::Rpc {
                                            msg: RpcMessage::InstallSnapshotResponse(reply),
                                            reply_tx: None,
                                        })
                                        .await;
                                    break;
                                }
                                Err(e) => {
                                    tracing::error!(
                                        "Failed to parse InstallSnapshotReply from {}: {}",
                                        url,
                                        e
                                    );
                                }
                            },
                            Err(e) => {
                                tracing::warn!(
                                    "InstallSnapshot failed to {} (attempt {}/{}): {}",
                                    url,
                                    attempt + 1,
                                    max_retries,
                                    e
                                );
                            }
                        }

                        attempt += 1;
                        if attempt >= max_retries {
                            break;
                        }
                        tokio::time::sleep(delay).await;
                        delay *= 2;
                    }
                });
                continue;
            }

            let prev_log_index = self.next_index[i] - 1 - self.last_included_index;
            let prev_log_term = self.log[prev_log_index].term;

            let next_log_index = self.next_index[i] - self.last_included_index;
            let entries = if self.log.len() > next_log_index {
                self.log[next_log_index..].to_vec()
            } else {
                vec![]
            };

            let args = AppendEntriesArgs {
                term: self.current_term,
                leader_id: self.id,
                prev_log_index: self.next_index[i] - 1,
                prev_log_term,
                entries,
                leader_commit: self.commit_index,
                leader_address: self.client_address.clone(),
            };

            let url = format!("{}/raft/append", peer);
            let tx = self.self_tx.clone();
            let client = self.http_client.clone();

            tokio::spawn(async move {
                // Heartbeats are frequent, so we might want fewer retries or shorter timeout
                // But for consistency, let's use a small retry loop
                let mut attempt = 0;
                let max_retries = 2; // Fewer retries for heartbeats
                let mut delay = Duration::from_millis(20);

                loop {
                    match client.post(&url).json(&args).send().await {
                        Ok(resp) => {
                            match resp.json::<AppendEntriesReply>().await {
                                Ok(reply) => {
                                    let _ = tx
                                        .send(Event::Rpc {
                                            msg: RpcMessage::AppendEntriesResponse(reply),
                                            reply_tx: None,
                                        })
                                        .await;
                                    break;
                                }
                                Err(_) => {
                                    // Don't log error for heartbeats to avoid spam
                                }
                            }
                        }

                        Err(e) => {
                            // Log error for debugging
                            tracing::warn!("AppendEntries failed to {}: {}", url, e);
                        }
                    }

                    attempt += 1;
                    if attempt >= max_retries {
                        break;
                    }
                    tokio::time::sleep(delay).await;
                    delay *= 2;
                }
            });
        }
        Ok(())
    }

    async fn handle_event(&mut self, event: Event) -> Result<()> {
        match event {
            Event::Rpc { msg, reply_tx } => {
                let response = self.handle_rpc(msg).await?;
                if let Some(tx) = reply_tx {
                    let _ = tx.send(response);
                }
            }
            Event::ClientRequest { command, reply_tx } => {
                tracing::info!(
                    "Node {} received ClientRequest (role: {:?})",
                    self.id,
                    self.role
                );
                if self.role != Role::Leader {
                    tracing::info!(
                        "Node {} rejecting ClientRequest because not leader (leader: {:?})",
                        self.id,
                        self.current_leader_address
                    );
                    let _ = reply_tx.send(Err(self.current_leader_address.clone()));
                    return Ok(());
                }

                let entry = LogEntry {
                    term: self.current_term,
                    command,
                };
                self.log.push(entry.clone());
                let absolute_index = self.log.len() - 1 + self.last_included_index;
                self.save_log_entry(absolute_index, &entry)?; // Persist log entry

                // Leader appends entry, but does not immediately commit it.
                // It will be committed once replicated to a majority.
                // The commit index advancement is handled after sending heartbeats
                // or in the AppendEntriesResponse handler.

                // For single-node clusters, we advance commit_index immediately.
                if self.peers.is_empty() && absolute_index > self.commit_index {
                    self.commit_index = absolute_index;
                    self.apply_logs();
                }

                // The original code had `self.commit_index = absolute_index; self.apply_logs();` here.
                // This is incorrect for a multi-node cluster as it commits before replication.
                // For single-node clusters, this is handled in `become_leader`.
                // For multi-node, commit_index is advanced in `handle_append_entries_response`.

                let _ = reply_tx.send(Ok(()));
            }
            Event::GetLeaderInfo { reply_tx } => {
                let _ = reply_tx.send(self.current_leader_address.clone());
            }
            Event::GetClusterInfo { reply_tx } => {
                let info = ClusterInfo {
                    node_id: self.id,
                    role: self.role,
                    current_term: self.current_term,
                    leader_id: self.current_leader,
                    leader_address: self.current_leader_address.clone(),
                    peers: self.peers.clone(),
                    commit_index: self.commit_index,
                    last_applied: self.last_applied,
                    log_len: self.log.len() + self.last_included_index,
                    votes_received: self.votes_received,
                    cluster_config: self.cluster_config.clone(),
                    config_change_state: self.config_change_state.clone(),
                };
                let _ = reply_tx.send(info);
            }
            Event::GetReadIndex { reply_tx } => {
                if self.role != Role::Leader {
                    let _ = reply_tx.send(Err(self.current_leader_address.clone()));
                    return Ok(());
                }

                // ReadIndex optimization:
                // 1. Record the current commit_index as the read_index.
                // 2. Add to pending_read_indices.
                // 3. Trigger immediate heartbeats to confirm leadership.

                let mut acks = HashSet::new();
                acks.insert(self.id); // Acknowledge self

                let majority_confirmed = self.cluster_config.has_joint_majority(&acks);

                self.pending_read_indices.push(ReadIndexRequest {
                    read_index: self.commit_index,
                    term: self.current_term,
                    acks,
                    majority_confirmed,
                    reply_tx,
                });

                if majority_confirmed {
                    self.check_read_indices();
                }

                // Trigger heartbeats immediately to speed up ReadIndex (for multi-node clusters)
                if !self.peers.is_empty() {
                    self.send_heartbeats().await?;
                }
            }
        }
        Ok(())
    }

    async fn handle_rpc(&mut self, msg: RpcMessage) -> Result<RpcMessage> {
        match msg {
            RpcMessage::RequestVote(args) => {
                tracing::info!(
                    "Node {} received RequestVote from Node {} for term {}",
                    self.id,
                    args.candidate_id,
                    args.term
                );
                let mut vote_granted = false;
                if args.term >= self.current_term {
                    if args.term > self.current_term {
                        tracing::info!(
                            "Node {} updating term to {} (was {})",
                            self.id,
                            args.term,
                            self.current_term
                        );
                        self.current_term = args.term;
                        self.save_term()?; // Persist term

                        self.role = Role::Follower;
                        self.voted_for = None;
                        self.save_vote()?; // Persist vote (None)

                        self.current_leader = None;
                        self.current_leader_address = None;
                    }

                    let last_log_index_local = self.log.len() - 1 + self.last_included_index;
                    let last_log_term_local = self.log[self.log.len() - 1].term;

                    // Raft election restriction: candidate's log must be at least as up-to-date
                    let log_is_up_to_date = args.last_log_term > last_log_term_local
                        || (args.last_log_term == last_log_term_local
                            && args.last_log_index >= last_log_index_local);

                    if (self.voted_for.is_none() || self.voted_for == Some(args.candidate_id))
                        && log_is_up_to_date
                    {
                        tracing::info!(
                            "Node {} granting vote to Node {} for term {}",
                            self.id,
                            args.candidate_id,
                            args.term
                        );
                        self.voted_for = Some(args.candidate_id);
                        self.save_vote()?; // Persist vote

                        self.last_election_time = Instant::now();
                        vote_granted = true;
                    } else {
                        tracing::info!(
                            "Node {} denying vote to Node {} (voted_for: {:?}, log_is_up_to_date: {})",
                            self.id,
                            args.candidate_id,
                            self.voted_for,
                            log_is_up_to_date
                        );
                    }
                } else {
                    tracing::info!(
                        "Node {} denying vote to Node {} (candidate term {} < current term {})",
                        self.id,
                        args.candidate_id,
                        args.term,
                        self.current_term
                    );
                }
                Ok(RpcMessage::RequestVoteResponse(RequestVoteReply {
                    term: self.current_term,
                    vote_granted,
                    peer_id: self.id,
                }))
            }
            RpcMessage::RequestVoteResponse(reply) => {
                if self.role == Role::Candidate
                    && self.current_term == reply.term
                    && reply.vote_granted
                {
                    tracing::info!("Node {} received vote from {}", self.id, reply.peer_id);
                    self.votes_received += 1;
                    self.voters.insert(reply.peer_id);
                    if self.cluster_config.has_joint_majority(&self.voters) {
                        self.become_leader();
                    }
                } else if reply.term > self.current_term {
                    tracing::info!(
                        "Node {} received higher term {} in RequestVoteResponse",
                        self.id,
                        reply.term
                    );
                    self.current_term = reply.term;
                    self.save_term()?; // Persist term

                    self.role = Role::Follower;
                    self.voted_for = None;
                    self.save_vote()?; // Persist vote

                    self.current_leader = None;
                    self.current_leader_address = None;
                }
                Ok(RpcMessage::RequestVoteResponse(reply))
            }
            RpcMessage::AppendEntries(args) => {
                let mut success = false;
                let mut match_index = 0;

                if args.term >= self.current_term {
                    if args.term > self.current_term {
                        tracing::info!(
                            "Node {} updating term to {} from AppendEntries RPC",
                            self.id,
                            args.term
                        );
                        self.current_term = args.term;
                        self.save_term()?;
                        self.voted_for = None; // Clear vote on new term
                        self.save_vote()?;
                    }

                    self.role = Role::Follower;
                    self.current_leader = Some(args.leader_id);
                    self.current_leader_address = Some(args.leader_address.clone());
                    self.last_election_time = Instant::now();

                    // Check if prev_log_index is in snapshot
                    if args.prev_log_index < self.last_included_index {
                        // Leader's prev_log_index is older than our snapshot,
                        // meaning we are ahead. This should not happen if leader is healthy.
                        // Or, leader needs to send a snapshot.
                        tracing::warn!(
                            "Node {} (F) prev_log_index {} < last_included_index {}. Leader needs to send snapshot.",
                            self.id, args.prev_log_index, self.last_included_index
                        );
                        success = false;
                        match_index = self.last_included_index; // Suggest leader to send snapshot from here
                    } else if args.prev_log_index == self.last_included_index {
                        // Prev log is the snapshot point
                        success = args.prev_log_term == self.last_included_term;
                        if success {
                            // Delete any conflicting entries starting from after snapshot
                            let log_start_index = 0; // Relative index in self.log
                            if self.log.len() > log_start_index {
                                // If there are entries after snapshot, check for conflicts
                                if self.log[log_start_index].term != args.prev_log_term {
                                    tracing::info!(
                                        "Node {} (F) conflicting entry at snapshot point. Truncating log.",
                                        self.id
                                    );
                                    self.log.truncate(log_start_index);
                                    self.delete_log_entries_from(self.last_included_index + 1)?;
                                }
                            }

                            // Append all entries
                            for (i, entry) in args.entries.iter().enumerate() {
                                let absolute_index = args.prev_log_index + 1 + i;
                                let log_index = absolute_index - self.last_included_index;

                                if log_index < self.log.len() {
                                    if self.log[log_index].term != entry.term {
                                        tracing::info!(
                                            "Node {} (F) conflicting entry at index {}. Truncating log.",
                                            self.id, absolute_index
                                        );
                                        self.log.truncate(log_index);
                                        self.delete_log_entries_from(absolute_index)?;

                                        self.log.push(entry.clone());
                                        self.save_log_entry(absolute_index, entry)?;
                                    }
                                } else {
                                    self.log.push(entry.clone());
                                    self.save_log_entry(absolute_index, entry)?;
                                }
                            }
                            match_index = args.prev_log_index + args.entries.len();
                        } else {
                            tracing::info!(
                                "Node {} (F) prev_log_term mismatch at snapshot point. Expected {}, got {}.",
                                self.id, self.last_included_term, args.prev_log_term
                            );
                        }
                    } else {
                        // Normal case: prev_log_index is after snapshot
                        let prev_log_index_local = args.prev_log_index - self.last_included_index;
                        if prev_log_index_local < self.log.len()
                            && self.log[prev_log_index_local].term == args.prev_log_term
                        {
                            success = true;

                            // Delete any conflicting entries
                            for (i, entry) in args.entries.iter().enumerate() {
                                let absolute_index = args.prev_log_index + 1 + i;
                                let log_index = absolute_index - self.last_included_index;

                                if log_index < self.log.len() {
                                    if self.log[log_index].term != entry.term {
                                        tracing::info!(
                                            "Node {} (F) conflicting entry at index {}. Truncating log.",
                                            self.id, absolute_index
                                        );
                                        self.log.truncate(log_index);
                                        self.delete_log_entries_from(absolute_index)?;
                                        break; // Stop checking, start appending from here
                                    }
                                } else {
                                    break; // No more local entries to check for conflict
                                }
                            }

                            // Append new entries (or re-append from conflict point)
                            for (i, entry) in args.entries.iter().enumerate() {
                                let absolute_index = args.prev_log_index + 1 + i;
                                let log_index = absolute_index - self.last_included_index;

                                if log_index >= self.log.len() {
                                    self.log.push(entry.clone());
                                    self.save_log_entry(absolute_index, entry)?;
                                }
                            }

                            match_index = args.prev_log_index + args.entries.len();
                        } else {
                            tracing::info!(
                                "Node {} (F) log mismatch: prev_log_index_local {} (log len {}), term mismatch (expected {}, got {}).",
                                self.id, prev_log_index_local, self.log.len(),
                                self.log.get(prev_log_index_local).map(|e| e.term).unwrap_or(0),
                                args.prev_log_term
                            );
                            success = false;
                            match_index = self.last_included_index; // Fallback to snapshot index
                            if prev_log_index_local < self.log.len() {
                                match_index = self.last_included_index + prev_log_index_local;
                            }
                        }
                    }

                    if success && args.leader_commit > self.commit_index {
                        self.commit_index = args
                            .leader_commit
                            .min(self.log.len() - 1 + self.last_included_index);
                        self.apply_logs();
                    }
                } else {
                    tracing::info!(
                        "Node {} (F) rejecting AppendEntries from leader {} (term {} < current term {})",
                        self.id, args.leader_id, args.term, self.current_term
                    );
                }
                Ok(RpcMessage::AppendEntriesResponse(AppendEntriesReply {
                    term: self.current_term,
                    success,
                    match_index,
                    peer_id: self.id,
                }))
            }
            RpcMessage::AppendEntriesResponse(reply) => {
                // Leader receives this - could update next_index/match_index here
                if self.role == Role::Leader && reply.term == self.current_term {
                    // Find the peer index
                    if let Some(peer_idx) = self.peers.iter().position(|p| {
                        // This is a simplification. In a real implementation,
                        // we'd need a better way to map addresses to IDs or
                        // include the peer's ID in the reply.
                        p.contains(&reply.peer_id.to_string())
                    }) {
                        if reply.success {
                            self.next_index[peer_idx] = reply.match_index + 1;
                            self.match_index[peer_idx] = reply.match_index;

                            // Update catch-up progress for non-voting members
                            if let Some(progress) = self.catch_up_progress.get_mut(&reply.peer_id) {
                                progress.update(reply.match_index);
                                tracing::debug!(
                                    "Non-voting member {} catch-up progress: match_index={}, rounds={}",
                                    reply.peer_id,
                                    progress.match_index,
                                    progress.rounds_caught_up
                                );
                            }

                            // Update ReadIndex acks
                            for req in self.pending_read_indices.iter_mut() {
                                if req.term == self.current_term {
                                    req.acks.insert(reply.peer_id);
                                    if self.cluster_config.has_joint_majority(&req.acks) {
                                        req.majority_confirmed = true;
                                    }
                                }
                            }
                            self.check_read_indices();

                            tracing::debug!(
                                "Node {} (L) updated peer {} next_index to {}, match_index to {}",
                                self.id,
                                reply.peer_id,
                                self.next_index[peer_idx],
                                self.match_index[peer_idx]
                            );
                        } else {
                            // Decrement next_index and retry
                            if self.next_index[peer_idx] > self.last_included_index + 1 {
                                self.next_index[peer_idx] -= 1;
                                tracing::info!(
                                    "Node {} (L) decremented peer {} next_index to {} due to AppendEntries failure",
                                    self.id, reply.peer_id, self.next_index[peer_idx]
                                );
                            } else {
                                // If next_index is already at last_included_index + 1,
                                // it means the follower is too far behind or needs a snapshot.
                                // The heartbeat logic should detect this and send a snapshot.
                                tracing::warn!(
                                    "Node {} (L) peer {} AppendEntries failed, next_index already at {}. Consider sending snapshot.",
                                    self.id, reply.peer_id, self.next_index[peer_idx]
                                );
                            }
                        }
                    }

                    // Check for commit_index advancement using joint consensus
                    // Build a map of server_id -> match_index
                    let mut server_match_indices = HashMap::new();
                    let absolute_index = self.log.len() - 1 + self.last_included_index;
                    server_match_indices.insert(self.id, absolute_index);

                    let all_members = self.cluster_config.all_members();
                    for (peer_idx, (server_id, _)) in all_members
                        .iter()
                        .filter(|(id, _)| **id != self.id)
                        .enumerate()
                    {
                        if peer_idx < self.match_index.len() {
                            server_match_indices.insert(*server_id, self.match_index[peer_idx]);
                        }
                    }

                    // Find the highest index that has a joint majority
                    let mut candidate_indices: Vec<usize> =
                        server_match_indices.values().copied().collect();
                    candidate_indices.sort_unstable();
                    candidate_indices.reverse();

                    let mut majority_match_index = self.commit_index;
                    for &candidate_index in &candidate_indices {
                        let mut acks = HashSet::new();
                        for (server_id, match_idx) in &server_match_indices {
                            if *match_idx >= candidate_index {
                                acks.insert(*server_id);
                            }
                        }
                        if self.cluster_config.has_joint_majority(&acks) {
                            majority_match_index = candidate_index;
                            break;
                        }
                    }

                    if majority_match_index > self.commit_index {
                        // Only commit if the entry is from the current term
                        let log_index_relative = majority_match_index - self.last_included_index;
                        if log_index_relative < self.log.len()
                            && self.log[log_index_relative].term == self.current_term
                        {
                            tracing::info!(
                                "Node {} (L) advancing commit_index to {}",
                                self.id,
                                majority_match_index
                            );
                            self.commit_index = majority_match_index;
                            self.apply_logs();
                        }
                    }
                } else if reply.term > self.current_term {
                    tracing::info!(
                        "Node {} (L) received higher term {} in AppendEntriesResponse",
                        self.id,
                        reply.term
                    );
                    self.current_term = reply.term;
                    self.save_term()?;
                    self.role = Role::Follower;
                    self.voted_for = None;
                    self.save_vote()?;
                    self.current_leader = None;
                    self.current_leader_address = None;
                }
                Ok(RpcMessage::AppendEntriesResponse(reply))
            }
            RpcMessage::InstallSnapshot(args) => {
                if args.term >= self.current_term {
                    if args.term > self.current_term {
                        tracing::info!(
                            "Node {} updating term to {} from InstallSnapshot RPC",
                            self.id,
                            args.term
                        );
                        self.current_term = args.term;
                        self.save_term()?;
                        self.voted_for = None; // Clear vote on new term
                        self.save_vote()?;
                    }

                    self.role = Role::Follower;
                    self.current_leader = Some(args.leader_id);
                    self.last_election_time = Instant::now();

                    // Only install if snapshot is newer than our current state
                    if args.last_included_index > self.last_included_index {
                        tracing::info!(
                            "Node {} (F) installing snapshot from leader {} at index {}",
                            self.id,
                            args.leader_id,
                            args.last_included_index
                        );
                        let snapshot = Snapshot {
                            last_included_index: args.last_included_index,
                            last_included_term: args.last_included_term,
                            data: args.data,
                        };
                        self.install_snapshot(snapshot)?;
                    } else {
                        tracing::info!(
                            "Node {} (F) rejecting snapshot from leader {} at index {} (current snapshot index {} is newer or same)",
                            self.id, args.leader_id, args.last_included_index, self.last_included_index
                        );
                    }
                } else {
                    tracing::info!(
                        "Node {} (F) rejecting InstallSnapshot from leader {} (term {} < current term {})",
                        self.id, args.leader_id, args.term, self.current_term
                    );
                }
                Ok(RpcMessage::InstallSnapshotResponse(InstallSnapshotReply {
                    term: self.current_term,
                    last_included_index: self.last_included_index,
                    peer_id: self.id,
                }))
            }
            RpcMessage::InstallSnapshotResponse(reply) => {
                // Leader receives this - snapshot transfer completed
                if self.role == Role::Leader && reply.term == self.current_term {
                    // Find the peer index
                    if let Some(peer_idx) = self.peers.iter().position(|p| {
                        // Extract peer ID from address (this is a simplification)
                        // In a real implementation, we'd need a better way to map addresses to IDs
                        p.contains(&reply.peer_id.to_string())
                    }) {
                        if reply.term == self.current_term {
                            // Update next_index and match_index
                            self.next_index[peer_idx] = reply.last_included_index + 1;
                            self.match_index[peer_idx] = reply.last_included_index;

                            // Update ReadIndex acks
                            for req in self.pending_read_indices.iter_mut() {
                                if req.term == self.current_term {
                                    req.acks.insert(reply.peer_id);
                                    if self.cluster_config.has_joint_majority(&req.acks) {
                                        req.majority_confirmed = true;
                                    }
                                }
                            }
                            self.check_read_indices();
                        }
                        tracing::info!(
                            "Node {} (L) peer {} successfully installed snapshot at index {}",
                            self.id,
                            reply.peer_id,
                            reply.last_included_index
                        );
                    }
                } else if reply.term > self.current_term {
                    tracing::info!(
                        "Node {} (L) received higher term {} in InstallSnapshotResponse",
                        self.id,
                        reply.term
                    );
                    self.current_term = reply.term;
                    self.save_term()?;
                    self.role = Role::Follower;
                    self.voted_for = None;
                    self.save_vote()?;
                    self.current_leader = None;
                    self.current_leader_address = None;
                }
                Ok(RpcMessage::InstallSnapshotResponse(reply))
            }
            RpcMessage::TimeoutNow(args) => {
                tracing::info!(
                    "Node {} received TimeoutNow from {} in term {}",
                    self.id,
                    args.sender_id,
                    args.term
                );

                if args.term < self.current_term {
                    return Ok(RpcMessage::TimeoutNowResponse(TimeoutNowReply {
                        term: self.current_term,
                        success: false,
                    }));
                }

                if args.term > self.current_term {
                    self.current_term = args.term;
                    self.save_term()?;
                    self.voted_for = None;
                    self.save_vote()?;
                    self.role = Role::Follower;
                }

                // Immediately start election
                tracing::info!("Starting immediate election due to TimeoutNow");
                self.role = Role::Candidate;
                self.current_term += 1;
                self.save_term()?;
                self.voted_for = Some(self.id);
                self.save_vote()?;
                self.votes_received = 1;
                self.voters = HashSet::new();
                self.voters.insert(self.id);
                self.last_election_time = Instant::now();

                Ok(RpcMessage::TimeoutNowResponse(TimeoutNowReply {
                    term: self.current_term,
                    success: true,
                }))
            }
            RpcMessage::TimeoutNowResponse(reply) => {
                // Fire-and-forget response, just log it
                tracing::debug!(
                    "Received TimeoutNowResponse: term={}, success={}",
                    reply.term,
                    reply.success
                );
                Ok(RpcMessage::TimeoutNowResponse(reply))
            }
        }
    }

    fn apply_logs(&mut self) {
        while self.commit_index > self.last_applied {
            self.last_applied += 1;
            let log_index = self.last_applied - self.last_included_index;
            if log_index < self.log.len() {
                let command = self.log[log_index].command.clone();

                // Handle membership commands specially - they modify RaftNode itself
                if let Command::Membership(ref cmd) = command {
                    self.apply_membership_command(cmd.clone());
                } else {
                    self.apply_command(&command);
                }
                // After applying a log entry, check if any pending read indices can be satisfied
                self.check_read_indices();
            } else {
                tracing::warn!(
                    "Warning: log_index {} >= log.len() {} during apply_logs. last_applied={}, last_included_index={}",
                    log_index, self.log.len(), self.last_applied, self.last_included_index
                );
            }
        }
    }

    fn apply_membership_command(&mut self, cmd: MembershipCommand) {
        match cmd {
            MembershipCommand::BeginJointConsensus {
                old_members,
                new_members,
                version,
            } => {
                tracing::info!(
                    "Applying joint consensus configuration (version {}): old={:?}, new={:?}",
                    version,
                    old_members,
                    new_members
                );

                self.cluster_config = ClusterConfiguration::Joint {
                    old_members,
                    new_members: new_members.clone(),
                    version,
                };

                // Update peers list and tracking arrays
                self.update_peer_tracking();

                // Save configuration to disk
                if let Err(e) = self.save_config() {
                    tracing::error!("Failed to save config after BeginJointConsensus: {}", e);
                }
            }

            MembershipCommand::FinalizeConfiguration {
                new_members,
                version,
            } => {
                tracing::info!(
                    "Finalizing configuration (version {}): members={:?}",
                    version,
                    new_members
                );

                self.cluster_config = ClusterConfiguration::Simple {
                    members: new_members,
                    version,
                };

                // Update peers list and tracking arrays
                self.update_peer_tracking();

                // Clear config change state
                self.config_change_state = ConfigChangeState::None;

                // Save configuration to disk
                if let Err(e) = self.save_config() {
                    tracing::error!("Failed to save config after FinalizeConfiguration: {}", e);
                }
            }

            MembershipCommand::AddServer {
                server_id,
                server_address,
            } => {
                tracing::warn!(
                    "AddServer command (legacy single-phase) for server {} ({}): deprecated, use joint consensus",
                    server_id,
                    server_address
                );
                // For backward compatibility, still apply it but log a warning
                // Check if server already exists
                if self.peers.iter().any(|p| p == &server_address) {
                    tracing::info!("Server {} already in cluster, skipping", server_address);
                    return;
                }

                tracing::info!(
                    "Adding server {} ({}) to cluster (legacy mode)",
                    server_id,
                    server_address
                );
                self.peers.push(server_address);

                // Reinitialize next_index and match_index for the new peer
                self.next_index
                    .push(self.log.len() + self.last_included_index);
                self.match_index.push(self.last_included_index);

                tracing::info!(
                    "Cluster now has {} peers: {:?}",
                    self.peers.len(),
                    self.peers
                );
            }

            MembershipCommand::RemoveServer { server_id } => {
                tracing::warn!(
                    "RemoveServer command (legacy single-phase) for server {}: deprecated, use joint consensus",
                    server_id
                );
                // For backward compatibility, still apply it but log a warning
                if server_id < self.peers.len() {
                    let removed = self.peers.remove(server_id);
                    self.next_index.remove(server_id);
                    self.match_index.remove(server_id);
                    tracing::info!(
                        "Removed server {} from cluster (legacy mode). Remaining peers: {:?}",
                        removed,
                        self.peers
                    );
                } else {
                    tracing::warn!(
                        "Cannot remove server {}: index out of bounds (have {} peers)",
                        server_id,
                        self.peers.len()
                    );
                }
            }

            MembershipCommand::AddServers { servers } => {
                tracing::warn!(
                    "AddServers command for {:?}: should not be applied directly, use joint consensus workflow",
                    servers
                );
            }

            MembershipCommand::RemoveServers { server_ids } => {
                tracing::warn!(
                    "RemoveServers command for {:?}: should not be applied directly, use joint consensus workflow",
                    server_ids
                );
            }
        }
    }

    /// Update peer tracking after configuration change
    fn update_peer_tracking(&mut self) {
        let all_members = self.cluster_config.all_members();

        // Rebuild peers list (excluding self)
        self.peers = all_members
            .iter()
            .filter(|(id, _)| **id != self.id)
            .map(|(_, addr)| addr.clone())
            .collect();

        // Resize next_index and match_index
        let new_size = self.peers.len();
        let default_next = self.log.len() + self.last_included_index;
        self.next_index.resize(new_size, default_next);
        self.match_index.resize(new_size, self.last_included_index);

        tracing::info!(
            "Updated peer tracking: {} peers, config version {}",
            new_size,
            self.cluster_config.version()
        );
    }

    /// Periodic check: promote non-voting members when caught up
    fn check_promote_non_voting_members(&mut self) -> Result<()> {
        if let ConfigChangeState::AddingServers { servers, .. } = &self.config_change_state {
            let mut all_caught_up = true;

            for (server_id, (_, progress)) in servers {
                if !progress.is_caught_up(self.commit_index) {
                    all_caught_up = false;
                    tracing::debug!(
                        "Server {} not yet caught up: match_index={}, commit_index={}, rounds={}",
                        server_id,
                        progress.match_index,
                        self.commit_index,
                        progress.rounds_caught_up
                    );
                }
            }

            if all_caught_up {
                tracing::info!("All new servers caught up, beginning joint consensus");
                self.promote_non_voting_members()?;
            }
        }

        Ok(())
    }

    /// Promote non-voting members to voting by starting joint consensus
    fn promote_non_voting_members(&mut self) -> Result<()> {
        let servers_to_promote: HashMap<usize, String> = match &self.config_change_state {
            ConfigChangeState::AddingServers { servers, .. } => servers
                .iter()
                .map(|(id, (addr, _))| (*id, addr.clone()))
                .collect(),
            _ => return Ok(()),
        };

        let old_members = match &self.cluster_config {
            ClusterConfiguration::Simple { members, .. } => members.clone(),
            _ => {
                return Err(anyhow::anyhow!(
                    "Cannot promote: already in joint consensus"
                ))
            }
        };

        let mut new_members = old_members.clone();
        for (id, addr) in servers_to_promote {
            new_members.insert(id, addr);
        }

        let version = self.cluster_config.version() + 1;

        // Append C-old,new
        let cmd = Command::Membership(MembershipCommand::BeginJointConsensus {
            old_members: old_members.clone(),
            new_members: new_members.clone(),
            version,
        });

        let entry = LogEntry {
            term: self.current_term,
            command: cmd,
        };

        self.log.push(entry);
        let absolute_index = self.log.len() - 1 + self.last_included_index;
        self.save_log_entry(absolute_index, &self.log[self.log.len() - 1])?;

        let joint_index = absolute_index;

        self.config_change_state = ConfigChangeState::InJointConsensus {
            joint_config_index: joint_index,
            target_config: new_members,
        };

        // Clear non-voting members (they're now in joint config)
        self.non_voting_members.clear();
        self.catch_up_progress.clear();

        tracing::info!(
            "Promoted non-voting members, entered joint consensus at index {}",
            joint_index
        );

        Ok(())
    }

    /// Check if joint consensus is committed and finalize
    fn check_finalize_joint_consensus(&mut self) -> Result<()> {
        if let ConfigChangeState::InJointConsensus {
            joint_config_index,
            target_config,
        } = &self.config_change_state
        {
            if self.commit_index >= *joint_config_index {
                tracing::info!(
                    "C-old,new committed at index {}, finalizing to C-new",
                    joint_config_index
                );

                let version = self.cluster_config.version() + 1;
                let target = target_config.clone();

                // Append C-new
                let cmd = Command::Membership(MembershipCommand::FinalizeConfiguration {
                    new_members: target,
                    version,
                });

                let entry = LogEntry {
                    term: self.current_term,
                    command: cmd,
                };

                self.log.push(entry);
                let absolute_index = self.log.len() - 1 + self.last_included_index;
                self.save_log_entry(absolute_index, &self.log[self.log.len() - 1])?;

                tracing::info!("Appended C-new at index {}", absolute_index);
            }
        }

        Ok(())
    }

    /// Initiate leader transfer to target server
    async fn initiate_leader_transfer(&mut self, target_id: usize) -> Result<(), String> {
        tracing::info!("Initiating leader transfer to server {}", target_id);

        // Find target in all members
        let all_members = self.cluster_config.all_members();
        let target_addr = all_members
            .get(&target_id)
            .ok_or_else(|| format!("Target server {} not found in cluster", target_id))?
            .clone();

        // Find target peer index
        let target_peer_idx = self
            .peers
            .iter()
            .position(|p| p == &target_addr)
            .ok_or_else(|| format!("Target server {} not in peers list", target_id))?;

        // Wait until target is caught up
        tracing::info!("Waiting for target server {} to catch up", target_id);
        let mut attempts = 0;
        let max_attempts = 50;

        while self.match_index[target_peer_idx] < self.log.len() - 1 + self.last_included_index {
            if attempts >= max_attempts {
                return Err(format!(
                    "Timeout waiting for target server {} to catch up (match_index={}, log_len={})",
                    target_id,
                    self.match_index[target_peer_idx],
                    self.log.len() - 1 + self.last_included_index
                ));
            }

            self.send_heartbeats()
                .await
                .map_err(|e| format!("Failed to replicate: {}", e))?;
            tokio::time::sleep(Duration::from_millis(100)).await;
            attempts += 1;
        }

        tracing::info!(
            "Target server {} is caught up (match_index={}), sending TimeoutNow",
            target_id,
            self.match_index[target_peer_idx]
        );

        // Send TimeoutNow RPC
        let args = TimeoutNowArgs {
            term: self.current_term,
            sender_id: self.id,
        };

        let url = format!("{}/raft/timeout_now", target_addr);
        let client = self.http_client.clone();

        // Send TimeoutNow (fire-and-forget)
        tokio::spawn(async move {
            match client.post(&url).json(&args).send().await {
                Ok(resp) => match resp.json::<TimeoutNowReply>().await {
                    Ok(reply) => {
                        tracing::info!(
                            "TimeoutNow sent successfully, reply: term={}, success={}",
                            reply.term,
                            reply.success
                        );
                    }
                    Err(e) => {
                        tracing::error!("Failed to parse TimeoutNowReply: {}", e);
                    }
                },
                Err(e) => {
                    tracing::error!("Failed to send TimeoutNow to {}: {}", url, e);
                }
            }
        });

        // Step down immediately
        tracing::info!(
            "Leader stepping down after initiating transfer to {}",
            target_id
        );
        self.role = Role::Follower;
        self.current_leader = None;
        self.current_leader_address = None;

        Ok(())
    }

    /// Handle a client request to add servers (public API)
    /// This implements the safe multi-phase protocol
    pub async fn handle_add_servers_request(
        &mut self,
        servers: HashMap<usize, String>,
    ) -> Result<(), String> {
        // Safety checks
        if self.role != Role::Leader {
            return Err("Not the leader".to_string());
        }

        if !matches!(self.config_change_state, ConfigChangeState::None) {
            return Err("Configuration change already in progress".to_string());
        }

        if servers.is_empty() {
            return Err("No servers to add".to_string());
        }

        // Verify servers don't already exist
        let current_members = self.cluster_config.all_members();
        for server_id in servers.keys() {
            if current_members.contains_key(server_id) {
                return Err(format!("Server {} already exists", server_id));
            }
        }

        tracing::info!("Starting add servers: {:?}", servers);

        // Phase 1: Add as non-voting members for catch-up
        self.config_change_state = ConfigChangeState::AddingServers {
            servers: servers
                .iter()
                .map(|(id, addr)| {
                    (
                        *id,
                        (addr.clone(), CatchUpProgress::new(self.monotonic_time)),
                    )
                })
                .collect(),
            started_at: self.monotonic_time,
        };

        self.non_voting_members.extend(servers.clone());
        for server_id in servers.keys() {
            self.server_roles.insert(*server_id, ServerRole::NonVoting);
            self.catch_up_progress
                .insert(*server_id, CatchUpProgress::new(self.monotonic_time));
        }

        // Extend tracking arrays for non-voting members
        for _ in 0..servers.len() {
            self.next_index
                .push(self.log.len() + self.last_included_index);
            self.match_index.push(self.last_included_index);
        }

        tracing::info!("Servers added as non-voting members, waiting for catch-up");
        Ok(())
    }

    /// Handle a client request to remove servers (public API)
    pub async fn handle_remove_servers_request(
        &mut self,
        server_ids: Vec<usize>,
    ) -> Result<(), String> {
        // Safety checks
        if self.role != Role::Leader {
            return Err("Not the leader".to_string());
        }

        if !matches!(self.config_change_state, ConfigChangeState::None) {
            return Err("Configuration change already in progress".to_string());
        }

        if server_ids.is_empty() {
            return Err("No servers to remove".to_string());
        }

        // Verify we're not removing too many servers
        let current_members = match &self.cluster_config {
            ClusterConfiguration::Simple { members, .. } => members,
            ClusterConfiguration::Joint { .. } => {
                return Err("Cannot remove servers during joint consensus".to_string());
            }
        };

        let remaining = current_members.len() - server_ids.len();
        if remaining < 1 {
            return Err("Cannot remove all servers".to_string());
        }

        // Check if we need to transfer leadership
        if server_ids.contains(&self.id) {
            tracing::info!("Leader is being removed, initiating leader transfer");

            // Find a suitable target (not being removed)
            let target = current_members
                .keys()
                .find(|id| !server_ids.contains(id) && **id != self.id)
                .copied();

            if let Some(target_id) = target {
                self.config_change_state = ConfigChangeState::TransferringLeadership {
                    target_server: target_id,
                    servers_to_remove: server_ids.clone(),
                };
                self.initiate_leader_transfer(target_id).await?;
                return Ok(());
            } else {
                return Err("No suitable target for leader transfer".to_string());
            }
        }

        // Proceed with removal via joint consensus
        self.begin_remove_servers_joint_consensus(server_ids).await
    }

    /// Begin joint consensus phase for removing servers
    async fn begin_remove_servers_joint_consensus(
        &mut self,
        server_ids: Vec<usize>,
    ) -> Result<(), String> {
        let old_members = match &self.cluster_config {
            ClusterConfiguration::Simple { members, .. } => members.clone(),
            _ => return Err("Invalid state".to_string()),
        };

        let mut new_members = old_members.clone();
        for server_id in &server_ids {
            new_members.remove(server_id);
        }

        let version = self.cluster_config.version() + 1;

        // Append C-old,new to log
        let cmd = Command::Membership(MembershipCommand::BeginJointConsensus {
            old_members: old_members.clone(),
            new_members: new_members.clone(),
            version,
        });

        let entry = LogEntry {
            term: self.current_term,
            command: cmd,
        };

        self.log.push(entry);
        let absolute_index = self.log.len() - 1 + self.last_included_index;
        self.save_log_entry(absolute_index, &self.log[self.log.len() - 1])
            .map_err(|e| format!("Failed to save log entry: {}", e))?;

        let joint_index = absolute_index;

        self.config_change_state = ConfigChangeState::InJointConsensus {
            joint_config_index: joint_index,
            target_config: new_members,
        };

        tracing::info!(
            "Appended C-old,new at index {}, version {}",
            joint_index,
            version
        );

        Ok(())
    }

    fn apply_command(&self, command: &Command) {
        let mut state = match self.app_state.lock() {
            Ok(s) => s,
            Err(e) => {
                tracing::error!("Failed to lock app_state in apply_command: {:?}", e);
                return;
            }
        };
        match command {
            Command::Master(cmd) => {
                if let AppState::Master(ref mut master_state) = *state {
                    match cmd {
                        MasterCommand::CreateFile { path } => {
                            master_state.files.insert(
                                path.clone(),
                                crate::dfs::FileMetadata {
                                    path: path.clone(),
                                    size: 0,
                                    blocks: vec![],
                                },
                            );
                            tracing::info!("Created file {}", path);
                        }
                        MasterCommand::DeleteFile { path } => {
                            if master_state.files.remove(path).is_some() {
                                tracing::info!("Deleted file {}", path);
                            }
                        }
                        MasterCommand::AllocateBlock {
                            path,
                            block_id,
                            locations,
                        } => {
                            if let Some(meta) = master_state.files.get_mut(path) {
                                meta.blocks.push(crate::dfs::BlockInfo {
                                    block_id: block_id.clone(),
                                    size: 0,
                                    locations: locations.clone(),
                                });
                                tracing::info!("Allocated block {} for file {}", block_id, path);
                            } else {
                                tracing::warn!("AllocateBlock: file {} not found", path);
                            }
                        }
                        MasterCommand::RegisterChunkServer { address: _ } => {
                            // ChunkServer registration is handled locally, not via Raft
                        }
                        // =================================================================
                        // Transaction Record Commands
                        // =================================================================
                        MasterCommand::RenameFile {
                            source_path,
                            dest_path,
                        } => {
                            // Same-shard rename: remove source, create dest with same metadata
                            if let Some(mut metadata) = master_state.files.remove(source_path) {
                                metadata.path = dest_path.clone();
                                master_state.files.insert(dest_path.clone(), metadata);
                                tracing::info!(
                                    "Renamed file from {} to {}",
                                    source_path,
                                    dest_path
                                );
                            } else {
                                tracing::warn!("RenameFile: source file {} not found", source_path);
                            }
                        }
                        MasterCommand::CreateTransactionRecord { record } => {
                            master_state
                                .transaction_records
                                .insert(record.tx_id.clone(), record.clone());
                            tracing::info!(
                                "Created transaction record: tx_id={}, state={:?}",
                                record.tx_id,
                                record.state
                            );
                        }
                        MasterCommand::UpdateTransactionState { tx_id, new_state } => {
                            if let Some(record) = master_state.transaction_records.get_mut(tx_id) {
                                let old_state = record.state.clone();
                                record.state = new_state.clone();
                                tracing::info!(
                                    "Updated transaction {} state: {:?} -> {:?}",
                                    tx_id,
                                    old_state,
                                    new_state
                                );
                            } else {
                                tracing::error!(
                                    "UpdateTransactionState: transaction {} not found",
                                    tx_id
                                );
                            }
                        }
                        MasterCommand::ApplyTransactionOperation { tx_id, operation } => {
                            // Apply the operation as part of a committed transaction
                            match &operation.op_type {
                                crate::master::TxOpType::Delete { path } => {
                                    if master_state.files.remove(path).is_some() {
                                        tracing::info!(
                                            "Transaction {}: deleted file {}",
                                            tx_id,
                                            path
                                        );
                                    } else {
                                        tracing::error!(
                                            "Transaction {}: failed to delete file {}: not found",
                                            tx_id,
                                            path
                                        );
                                    }
                                }
                                crate::master::TxOpType::Create { path, metadata } => {
                                    master_state.files.insert(path.clone(), metadata.clone());
                                    tracing::info!("Transaction {}: created file {}", tx_id, path);
                                }
                            }
                        }
                        MasterCommand::DeleteTransactionRecord { tx_id } => {
                            if master_state.transaction_records.remove(tx_id).is_some() {
                                tracing::info!("Deleted transaction record: tx_id={}", tx_id);
                            } else {
                                eprintln!(
                                    "DeleteTransactionRecord: transaction {} not found",
                                    tx_id
                                );
                            }
                        }
                        MasterCommand::SplitShard {
                            split_key,
                            new_shard_id,
                            new_shard_peers: _,
                        } => {
                            // Collect files that belong to the new shard (lexicographical >= split_key)
                            let to_move: Vec<String> = master_state
                                .files
                                .keys()
                                .filter(|k| **k >= *split_key)
                                .cloned()
                                .collect();

                            let count = to_move.len();
                            for path in to_move {
                                master_state.files.remove(&path);
                            }

                            tracing::info!(count, new_shard_id);
                        }
                        MasterCommand::MergeShard { victim_shard_id } => {
                            tracing::info!(
                                "Shard merge: {} is being merged into this shard",
                                victim_shard_id
                            );
                            // Note: Actual metadata ingestion is handled via IngestBatch RPC
                            // called by the victim shard's master after the merge is committed.
                        }
                        MasterCommand::IngestBatch { files } => {
                            let count = files.len();
                            for file in files {
                                master_state.files.insert(file.path.clone(), file.clone());
                            }
                            tracing::info!("Ingested batch of {} files into shard", count);
                        }
                        MasterCommand::TriggerShuffle { prefix } => {
                            master_state.shuffling_prefixes.insert(prefix.clone());
                            tracing::info!("Triggered background shuffling for prefix: {}", prefix);
                        }
                        MasterCommand::CompleteFile { path, size } => {
                            if let Some(file) = master_state.files.get_mut(path) {
                                file.size = *size;

                                // Update block sizes based on total file size
                                // Distribute size across blocks: each block gets equal size except last
                                let num_blocks = file.blocks.len();
                                if num_blocks > 0 {
                                    let remaining_size = *size;

                                    // For single block, assign all size to it
                                    // For multiple blocks, we'll just mark the first block with full size
                                    // This is a simplified approach - ideally blocks should report their actual size
                                    if num_blocks == 1 {
                                        file.blocks[0].size = remaining_size;
                                    } else {
                                        // For multiple blocks, distribute evenly
                                        // This is an approximation since we don't know actual block boundaries
                                        let block_size = remaining_size / num_blocks as u64;
                                        let last_block_size =
                                            remaining_size - (block_size * (num_blocks - 1) as u64);

                                        for i in 0..num_blocks - 1 {
                                            file.blocks[i].size = block_size;
                                        }
                                        file.blocks[num_blocks - 1].size = last_block_size;
                                    }
                                }

                                tracing::info!("Completed file {}: size={} bytes", path, size);
                            }
                        }
                        MasterCommand::StopShuffle { prefix } => {
                            master_state.shuffling_prefixes.remove(prefix.as_str());
                            tracing::info!("Stopped background shuffling for prefix: {}", prefix);
                        }
                    }
                } else {
                    tracing::error!("Error: Received MasterCommand but state is not MasterState");
                }
            }
            Command::Config(cmd) => {
                if let AppState::Config(ref mut config_state) = *state {
                    match cmd {
                        ConfigCommand::AddShard { shard_id, peers } => {
                            config_state
                                .shard_map
                                .add_shard(shard_id.clone(), peers.clone());
                        }
                        ConfigCommand::RemoveShard { shard_id } => {
                            config_state.shard_map.remove_shard(shard_id);
                        }
                        ConfigCommand::SplitShard {
                            shard_id: _,
                            split_key,
                            new_shard_id,
                            new_shard_peers,
                        } => {
                            config_state.shard_map.split_shard(
                                split_key.clone(),
                                new_shard_id.clone(),
                                new_shard_peers.clone(),
                            );
                        }
                        ConfigCommand::MergeShard {
                            victim_shard_id,
                            retained_shard_id,
                        } => {
                            config_state
                                .shard_map
                                .merge_shards(victim_shard_id, retained_shard_id);
                        }
                        ConfigCommand::RebalanceShard { old_key, new_key } => {
                            config_state
                                .shard_map
                                .rebalance_boundary(old_key.clone(), new_key.clone());
                        }
                        ConfigCommand::RegisterMaster { address, shard_id } => {
                            // Automatically add the shard to the map if it doesn't exist.
                            // This simplifies initial cluster setup.
                            if !config_state.shard_map.has_shard(shard_id) {
                                tracing::info!(
                                    "RegisterMaster: Adding new shard {} to ShardMap",
                                    shard_id
                                );
                                config_state
                                    .shard_map
                                    .add_shard(shard_id.clone(), vec![address.clone()]);
                            } else {
                                // Add this address to the shard's peers if not already present
                                let mut peers = config_state
                                    .shard_map
                                    .get_peers(shard_id)
                                    .unwrap_or_default();
                                if !peers.contains(address) {
                                    peers.push(address.clone());
                                    config_state.shard_map.add_shard(shard_id.clone(), peers);
                                }
                            }

                            config_state.masters.insert(
                                address.clone(),
                                MasterInfo {
                                    address: address.clone(),
                                    shard_id: shard_id.clone(),
                                    last_heartbeat: std::time::SystemTime::now()
                                        .duration_since(std::time::UNIX_EPOCH)
                                        .unwrap()
                                        .as_secs(),
                                    rps_per_prefix: HashMap::new(),
                                },
                            );
                        }
                        ConfigCommand::ShardHeartbeat {
                            address,
                            rps_per_prefix,
                        } => {
                            if let Some(info) = config_state.masters.get_mut(address) {
                                info.last_heartbeat = std::time::SystemTime::now()
                                    .duration_since(std::time::UNIX_EPOCH)
                                    .unwrap()
                                    .as_secs();
                                info.rps_per_prefix = rps_per_prefix.clone();
                            }
                        }
                    }
                } else {
                    tracing::error!("Error: Received ConfigCommand but state is not ConfigState");
                }
            }
            Command::Membership(cmd) => {
                // Membership commands are handled separately in apply_membership_command
                // They modify the RaftNode's peer list, not the application state
                tracing::info!("Membership command applied via log: {:?}", cmd);
            }
            Command::NoOp => {}
        }
    }
}

// ============================================================================
// Unit Tests
// ============================================================================
#[cfg(test)]
mod tests {
    use super::*;
    use crate::master::{TransactionRecord, TxOpType, TxOperation, TxState};

    #[test]
    fn test_master_command_create_file() {
        let cmd = MasterCommand::CreateFile {
            path: "/test/file.txt".to_string(),
        };

        match cmd {
            MasterCommand::CreateFile { path } => {
                assert_eq!(path, "/test/file.txt");
            }
            _ => panic!("Expected CreateFile command"),
        }
    }

    #[test]
    fn test_master_command_rename_file() {
        let cmd = MasterCommand::RenameFile {
            source_path: "/source.txt".to_string(),
            dest_path: "/dest.txt".to_string(),
        };

        match cmd {
            MasterCommand::RenameFile {
                source_path,
                dest_path,
            } => {
                assert_eq!(source_path, "/source.txt");
                assert_eq!(dest_path, "/dest.txt");
            }
            _ => panic!("Expected RenameFile command"),
        }
    }

    #[test]
    fn test_master_command_create_transaction_record() {
        let metadata = crate::dfs::FileMetadata {
            path: "/dest.txt".to_string(),
            size: 100,
            blocks: vec![],
        };

        let tx_record = TransactionRecord::new_rename(
            "tx-123".to_string(),
            "/source.txt".to_string(),
            "/dest.txt".to_string(),
            "shard-1".to_string(),
            "shard-2".to_string(),
            metadata,
        );

        let cmd = MasterCommand::CreateTransactionRecord {
            record: tx_record.clone(),
        };

        match cmd {
            MasterCommand::CreateTransactionRecord { record } => {
                assert_eq!(record.tx_id, "tx-123");
                assert_eq!(record.state, TxState::Pending);
            }
            _ => panic!("Expected CreateTransactionRecord command"),
        }
    }

    #[test]
    fn test_master_command_update_transaction_state() {
        let cmd = MasterCommand::UpdateTransactionState {
            tx_id: "tx-123".to_string(),
            new_state: TxState::Committed,
        };

        match cmd {
            MasterCommand::UpdateTransactionState { tx_id, new_state } => {
                assert_eq!(tx_id, "tx-123");
                assert_eq!(new_state, TxState::Committed);
            }
            _ => panic!("Expected UpdateTransactionState command"),
        }
    }

    #[test]
    fn test_master_command_apply_transaction_operation() {
        let operation = TxOperation {
            shard_id: "shard-1".to_string(),
            op_type: TxOpType::Delete {
                path: "/old/file.txt".to_string(),
            },
        };

        let cmd = MasterCommand::ApplyTransactionOperation {
            tx_id: "tx-123".to_string(),
            operation: operation.clone(),
        };

        match cmd {
            MasterCommand::ApplyTransactionOperation { tx_id, operation } => {
                assert_eq!(tx_id, "tx-123");
                assert_eq!(operation.shard_id, "shard-1");
            }
            _ => panic!("Expected ApplyTransactionOperation command"),
        }
    }

    #[test]
    fn test_master_command_delete_transaction_record() {
        let cmd = MasterCommand::DeleteTransactionRecord {
            tx_id: "tx-123".to_string(),
        };

        match cmd {
            MasterCommand::DeleteTransactionRecord { tx_id } => {
                assert_eq!(tx_id, "tx-123");
            }
            _ => panic!("Expected DeleteTransactionRecord command"),
        }
    }

    #[test]
    fn test_command_serialization() {
        let cmd = Command::Master(MasterCommand::RenameFile {
            source_path: "/source.txt".to_string(),
            dest_path: "/dest.txt".to_string(),
        });

        // Test that it can be serialized and deserialized
        let serialized = serde_json::to_string(&cmd).expect("Failed to serialize command");
        let deserialized: Command =
            serde_json::from_str(&serialized).expect("Failed to deserialize command");

        match deserialized {
            Command::Master(MasterCommand::RenameFile {
                source_path,
                dest_path,
            }) => {
                assert_eq!(source_path, "/source.txt");
                assert_eq!(dest_path, "/dest.txt");
            }
            _ => panic!("Deserialization produced wrong command type"),
        }
    }

    #[test]
    fn test_log_entry_serialization() {
        let entry = LogEntry {
            term: 1,
            command: Command::Master(MasterCommand::CreateFile {
                path: "/test.txt".to_string(),
            }),
        };

        let serialized = serde_json::to_string(&entry).expect("Failed to serialize log entry");
        let deserialized: LogEntry =
            serde_json::from_str(&serialized).expect("Failed to deserialize log entry");

        assert_eq!(deserialized.term, 1);
    }
}
