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

use crate::master::MasterState;
use crate::sharding::ShardMap;
use anyhow::{Context, Result};
use rand::Rng;
use rocksdb::DB;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
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
    /// Add a new server to the Raft cluster
    AddServer {
        /// Server ID (typically the node index)
        server_id: usize,
        /// Server address (HTTP endpoint for Raft RPC)
        server_address: String,
    },
    /// Remove a server from the Raft cluster
    RemoveServer {
        /// Server ID to remove
        server_id: usize,
    },
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
    /// Split current shard into two at the given split point (lexicographical).
    SplitShard {
        split_key: String,
        new_shard_id: String,
        new_shard_peers: Vec<String>,
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
}

#[derive(Debug, Serialize, Deserialize)]
pub enum AppState {
    Master(MasterState),
    Config(ShardMap),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    pub last_included_index: usize,
    pub last_included_term: u64,
    pub data: Vec<u8>, // Serialized MasterState
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum RpcMessage {
    RequestVote(RequestVoteArgs),
    RequestVoteResponse(RequestVoteReply),
    AppendEntries(AppendEntriesArgs),
    AppendEntriesResponse(AppendEntriesReply),
    InstallSnapshot(InstallSnapshotArgs),
    InstallSnapshotResponse(InstallSnapshotReply),
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
            election_timeout: Duration::from_millis(rand::rng().random_range(1500..3000)),
            last_election_time: Instant::now(),
            inbox,
            self_tx,
            app_state,
            votes_received: 0,
            db,
            http_client,
            pending_read_indices: vec![],
            last_heartbeat_time: Instant::now(),
            last_majority_ack_time: Instant::now(),
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
        match self.role {
            Role::Follower | Role::Candidate => {
                if self.last_election_time.elapsed() > self.election_timeout {
                    self.start_election().await?;
                }
            }
            Role::Leader => {
                self.send_heartbeats().await?;
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
        self.last_election_time = Instant::now();
        self.election_timeout = Duration::from_millis(rand::rng().random_range(1500..3000));

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

        let total_nodes = self.peers.len() + 1;
        if total_nodes == 1 {
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
        for (i, peer) in self.peers.iter().enumerate() {
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

                let total_nodes = self.peers.len() + 1;
                let majority_confirmed = acks.len() > total_nodes / 2;

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
                }))
            }
            RpcMessage::RequestVoteResponse(reply) => {
                if self.role == Role::Candidate
                    && self.current_term == reply.term
                    && reply.vote_granted
                {
                    tracing::info!("Node {} received vote", self.id);
                    self.votes_received += 1;
                    let total_nodes = self.peers.len() + 1;
                    if self.votes_received > total_nodes / 2 {
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

                            // Update ReadIndex acks
                            let total_nodes = self.peers.len() + 1;
                            for req in self.pending_read_indices.iter_mut() {
                                if req.term == self.current_term {
                                    req.acks.insert(reply.peer_id);
                                    if req.acks.len() > total_nodes / 2 {
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

                    // Check for commit_index advancement
                    // Include leader's own match index (which is absolute_index)
                    let mut all_match_indices = self.match_index.clone();
                    let absolute_index = self.log.len() - 1 + self.last_included_index;
                    all_match_indices.push(absolute_index);
                    all_match_indices.sort_unstable();

                    let total_nodes = self.peers.len() + 1;
                    let majority_match_index = all_match_indices[total_nodes / 2];

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
                            let total_nodes = self.peers.len() + 1;
                            for req in self.pending_read_indices.iter_mut() {
                                if req.term == self.current_term {
                                    req.acks.insert(reply.peer_id);
                                    if req.acks.len() > total_nodes / 2 {
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
            MembershipCommand::AddServer {
                server_id,
                server_address,
            } => {
                // Check if server already exists
                if self.peers.iter().any(|p| p == &server_address) {
                    tracing::info!("Server {} already in cluster, skipping", server_address);
                    return;
                }

                tracing::info!(
                    "Adding server {} ({}) to cluster",
                    server_id,
                    server_address
                );
                self.peers.push(server_address);

                // Reinitialize next_index and match_index for the new peer
                self.next_index
                    .push(self.log.len() + self.last_included_index); // next_index for new peer should be leader's last log index + 1
                self.match_index.push(self.last_included_index); // match_index for new peer should be 0 (or last_included_index)

                tracing::info!(
                    "Cluster now has {} peers: {:?}",
                    self.peers.len(),
                    self.peers
                );
            }
            MembershipCommand::RemoveServer { server_id } => {
                // Find and remove the server
                if server_id < self.peers.len() {
                    let removed = self.peers.remove(server_id);
                    self.next_index.remove(server_id);
                    self.match_index.remove(server_id);
                    tracing::info!(
                        "Removed server {} from cluster. Remaining peers: {:?}",
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
        }
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

                            tracing::info!(
                                "Shard split at {}: removed {} files to new shard {}",
                                split_key,
                                count,
                                new_shard_id
                            );
                        }
                    }
                } else {
                    tracing::error!("Error: Received MasterCommand but state is not MasterState");
                }
            }
            Command::Config(cmd) => {
                if let AppState::Config(ref mut shard_map) = *state {
                    match cmd {
                        ConfigCommand::AddShard { shard_id, peers } => {
                            shard_map.add_shard(shard_id.clone(), peers.clone());
                        }
                        ConfigCommand::RemoveShard { shard_id } => {
                            shard_map.remove_shard(shard_id);
                        }
                        ConfigCommand::SplitShard {
                            shard_id: _,
                            split_key,
                            new_shard_id,
                            new_shard_peers,
                        } => {
                            shard_map.split_shard(
                                split_key.clone(),
                                new_shard_id.clone(),
                                new_shard_peers.clone(),
                            );
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
