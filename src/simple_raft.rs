use crate::master::MasterState;
use crate::sharding::ShardMap;
use rand::Rng;
use rocksdb::DB;
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;
use tokio::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
    NoOp,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum MasterCommand {
    CreateFile {
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
}

pub struct RaftNode {
    pub id: usize,
    pub peers: Vec<String>,
    pub client_address: String,
    pub role: Role,
    pub current_term: u64,
    pub voted_for: Option<usize>,
    pub log: Vec<LogEntry>,
    pub commit_index: usize,
    pub last_applied: usize,
    pub next_index: Vec<usize>,
    pub match_index: Vec<usize>,
    pub current_leader: Option<usize>,
    pub current_leader_address: Option<String>,

    // Snapshot metadata
    pub last_included_index: usize,
    pub last_included_term: u64,

    pub election_timeout: Duration,
    pub last_election_time: Instant,

    pub inbox: mpsc::Receiver<Event>,
    pub self_tx: mpsc::Sender<Event>,
    pub app_state: Arc<Mutex<AppState>>,
    pub votes_received: usize,

    pub db: Arc<DB>, // RocksDB instance
    pub http_client: reqwest::Client,
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
            .unwrap();

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
                    eprintln!("Error: Corrupted term data in DB");
                    0
                }
            },
            _ => 0,
        };
        let voted_for = match db.get(b"vote") {
            Ok(Some(val)) => match val.try_into() {
                Ok(bytes) => Some(usize::from_be_bytes(bytes)),
                Err(_) => {
                    eprintln!("Error: Corrupted vote data in DB");
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
                                    *app_state.lock().unwrap() = state;
                                }
                                Err(_) => {
                                    // Try legacy MasterState
                                    match serde_json::from_slice::<MasterState>(&snapshot_data) {
                                        Ok(state) => {
                                            *app_state.lock().unwrap() = AppState::Master(state);
                                        }
                                        Err(e) => {
                                            eprintln!(
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
                        eprintln!("Error: Failed to deserialize snapshot metadata: {}", e);
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
                        eprintln!(
                            "Error: Failed to deserialize log entry at index {}: {}",
                            index, e
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

    fn save_term(&self) {
        self.db
            .put(b"term", self.current_term.to_be_bytes())
            .expect("Failed to save term to DB");
    }

    fn save_vote(&self) {
        if let Some(vote) = self.voted_for {
            self.db
                .put(b"vote", vote.to_be_bytes())
                .expect("Failed to save vote to DB");
        } else {
            self.db
                .delete(b"vote")
                .expect("Failed to delete vote from DB");
        }
    }

    fn save_log_entry(&self, index: usize, entry: &LogEntry) {
        let key = format!("log:{}", index);
        let val = serde_json::to_vec(entry).expect("Failed to serialize log entry");
        self.db
            .put(key.as_bytes(), val)
            .expect("Failed to save log entry to DB");
    }

    fn delete_log_entries_from(&self, start_index: usize) {
        let mut index = start_index;
        loop {
            let key = format!("log:{}", index);
            if self
                .db
                .get(key.as_bytes())
                .expect("Failed to read from DB")
                .is_none()
            {
                break;
            }
            self.db
                .delete(key.as_bytes())
                .expect("Failed to delete from DB");
            index += 1;
        }
    }

    fn create_snapshot(&mut self) {
        // Serialize current app state
        let state = self.app_state.lock().unwrap();
        let snapshot_data = serde_json::to_vec(&*state).expect("Failed to serialize app state");
        drop(state);

        // Save snapshot metadata
        let term = if self.last_applied >= self.last_included_index {
            let index = self.last_applied - self.last_included_index;
            self.log
                .get(index)
                .map(|e| e.term)
                .unwrap_or(self.last_included_term)
        } else {
            eprintln!(
                "Warning: last_applied {} < last_included_index {} during snapshot creation",
                self.last_applied, self.last_included_index
            );
            self.last_included_term
        };
        let meta = (self.last_applied, term);
        let meta_bytes = serde_json::to_vec(&meta).expect("Failed to serialize snapshot metadata");
        self.db
            .put(b"snapshot_meta", meta_bytes)
            .expect("Failed to save snapshot metadata to DB");

        // Save snapshot data
        self.db
            .put(b"snapshot_data", snapshot_data)
            .expect("Failed to save snapshot data to DB");

        // Delete old log entries
        for i in (self.last_included_index + 1)..=self.last_applied {
            let key = format!("log:{}", i);
            self.db
                .delete(key.as_bytes())
                .expect("Failed to delete old log entry from DB");
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

        println!(
            "Node {} created snapshot at index {}",
            self.id, self.last_included_index
        );
    }

    fn install_snapshot(&mut self, snapshot: Snapshot) {
        // Save snapshot to disk
        let meta = (snapshot.last_included_index, snapshot.last_included_term);
        let meta_bytes = serde_json::to_vec(&meta).expect("Failed to serialize snapshot metadata");
        self.db
            .put(b"snapshot_meta", meta_bytes)
            .expect("Failed to save snapshot metadata to DB");
        self.db
            .put(b"snapshot_data", &snapshot.data)
            .expect("Failed to save snapshot data to DB");

        // Restore app state
        match serde_json::from_slice::<AppState>(&snapshot.data) {
            Ok(state) => *self.app_state.lock().unwrap() = state,
            Err(_) => {
                // Try legacy MasterState
                match serde_json::from_slice::<MasterState>(&snapshot.data) {
                    Ok(state) => *self.app_state.lock().unwrap() = AppState::Master(state),
                    Err(e) => eprintln!("Error: Failed to deserialize snapshot data: {}", e),
                }
            }
        }

        // Delete old log entries
        for i in (self.last_included_index + 1)..=snapshot.last_included_index {
            let key = format!("log:{}", i);
            self.db
                .delete(key.as_bytes())
                .expect("Failed to delete old log entry from DB");
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

        println!(
            "Node {} installed snapshot at index {}",
            self.id, self.last_included_index
        );
    }

    pub async fn run(&mut self) {
        let tick_rate = Duration::from_millis(100);
        let mut tick_interval = tokio::time::interval(tick_rate);

        self.next_index = vec![self.log.len() + self.last_included_index; self.peers.len()];
        self.match_index = vec![self.last_included_index; self.peers.len()];

        loop {
            tokio::select! {
                _ = tick_interval.tick() => {
                    self.tick().await;
                }
                Some(event) = self.inbox.recv() => {
                    self.handle_event(event).await;
                }
            }
        }
    }

    async fn tick(&mut self) {
        match self.role {
            Role::Follower | Role::Candidate => {
                if self.last_election_time.elapsed() > self.election_timeout {
                    self.start_election().await;
                }
            }
            Role::Leader => {
                self.send_heartbeats().await;
            }
        }
        self.apply_logs();

        // Create snapshot if log is too large (threshold: 100 entries)
        const SNAPSHOT_THRESHOLD: usize = 100;
        if self.log.len() > SNAPSHOT_THRESHOLD && self.last_applied > self.last_included_index {
            self.create_snapshot();
        }
    }

    async fn start_election(&mut self) {
        self.role = Role::Candidate;
        self.current_term += 1;
        self.save_term(); // Persist term

        self.voted_for = Some(self.id);
        self.save_vote(); // Persist vote

        self.votes_received = 1;
        self.last_election_time = Instant::now();
        self.election_timeout = Duration::from_millis(rand::rng().random_range(1500..3000));

        println!(
            "Node {} starting election for term {}",
            self.id, self.current_term
        );

        let last_log_index = self.log.len() - 1;
        let last_log_term = self.log[last_log_index].term;

        let args = RequestVoteArgs {
            term: self.current_term,
            candidate_id: self.id,
            last_log_index,
            last_log_term,
        };

        let total_nodes = self.peers.len() + 1;
        if total_nodes == 1 {
            self.become_leader();
            return;
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
                                eprintln!(
                                    "Failed to parse RequestVoteResponse from {}: {}",
                                    url, e
                                );
                            }
                        },
                        Err(e) => {
                            eprintln!(
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
    }

    fn become_leader(&mut self) {
        println!(
            "Node {} becoming LEADER for term {}",
            self.id, self.current_term
        );
        self.role = Role::Leader;
        self.current_leader = Some(self.id);
        self.current_leader_address = Some(self.client_address.clone());
        self.next_index = vec![self.log.len() + self.last_included_index; self.peers.len()];
        self.match_index = vec![self.last_included_index; self.peers.len()];
    }

    async fn send_heartbeats(&mut self) {
        for (i, peer) in self.peers.iter().enumerate() {
            // Check if follower needs a snapshot
            if self.next_index[i] <= self.last_included_index {
                // Send snapshot instead of AppendEntries
                let state = self.app_state.lock().unwrap();
                let snapshot_data = serde_json::to_vec(&*state).unwrap();
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
                                    eprintln!(
                                        "Failed to parse InstallSnapshotReply from {}: {}",
                                        url, e
                                    );
                                }
                            },
                            Err(e) => {
                                eprintln!(
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
                            eprintln!("AppendEntries failed to {}: {}", url, e);
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
    }

    async fn handle_event(&mut self, event: Event) {
        match event {
            Event::Rpc { msg, reply_tx } => {
                let response = self.handle_rpc(msg).await;
                if let Some(tx) = reply_tx {
                    let _ = tx.send(response);
                }
            }
            Event::ClientRequest { command, reply_tx } => {
                println!(
                    "Node {} received ClientRequest (role: {:?})",
                    self.id, self.role
                );
                if self.role != Role::Leader {
                    println!(
                        "Node {} rejecting ClientRequest because not leader (leader: {:?})",
                        self.id, self.current_leader_address
                    );
                    let _ = reply_tx.send(Err(self.current_leader_address.clone()));
                    return;
                }

                let entry = LogEntry {
                    term: self.current_term,
                    command,
                };
                self.log.push(entry.clone());
                let absolute_index = self.log.len() - 1 + self.last_included_index;
                self.save_log_entry(absolute_index, &entry); // Persist log entry

                self.commit_index = absolute_index;
                self.apply_logs();

                let _ = reply_tx.send(Ok(()));
            }
            Event::GetLeaderInfo { reply_tx } => {
                let _ = reply_tx.send(self.current_leader_address.clone());
            }
        }
    }

    async fn handle_rpc(&mut self, msg: RpcMessage) -> RpcMessage {
        match msg {
            RpcMessage::RequestVote(args) => {
                println!(
                    "Node {} received RequestVote from Node {} for term {}",
                    self.id, args.candidate_id, args.term
                );
                let mut vote_granted = false;
                if args.term >= self.current_term {
                    if args.term > self.current_term {
                        println!(
                            "Node {} updating term to {} (was {})",
                            self.id, args.term, self.current_term
                        );
                        self.current_term = args.term;
                        self.save_term(); // Persist term

                        self.role = Role::Follower;
                        self.voted_for = None;
                        self.save_vote(); // Persist vote (None)

                        self.current_leader = None;
                        self.current_leader_address = None;
                    }

                    if (self.voted_for.is_none() || self.voted_for == Some(args.candidate_id))
                        && args.last_log_index >= self.log.len() - 1 + self.last_included_index
                    {
                        println!(
                            "Node {} granting vote to Node {}",
                            self.id, args.candidate_id
                        );
                        self.voted_for = Some(args.candidate_id);
                        self.save_vote(); // Persist vote

                        self.last_election_time = Instant::now();
                        vote_granted = true;
                    } else {
                        println!(
                            "Node {} denying vote to Node {} (voted_for: {:?}, log check: {})",
                            self.id,
                            args.candidate_id,
                            self.voted_for,
                            args.last_log_index >= self.log.len() - 1 + self.last_included_index
                        );
                    }
                } else {
                    println!(
                        "Node {} denying vote to Node {} (term {} < {})",
                        self.id, args.candidate_id, args.term, self.current_term
                    );
                }
                RpcMessage::RequestVoteResponse(RequestVoteReply {
                    term: self.current_term,
                    vote_granted,
                })
            }
            RpcMessage::RequestVoteResponse(reply) => {
                if self.role == Role::Candidate
                    && self.current_term == reply.term
                    && reply.vote_granted
                {
                    println!("Node {} received vote", self.id);
                    self.votes_received += 1;
                    let total_nodes = self.peers.len() + 1;
                    if self.votes_received > total_nodes / 2 {
                        self.become_leader();
                    }
                } else if reply.term > self.current_term {
                    println!(
                        "Node {} received higher term {} in RequestVoteResponse",
                        self.id, reply.term
                    );
                    self.current_term = reply.term;
                    self.save_term(); // Persist term

                    self.role = Role::Follower;
                    self.voted_for = None;
                    self.save_vote(); // Persist vote

                    self.current_leader = None;
                    self.current_leader_address = None;
                }
                RpcMessage::RequestVoteResponse(reply)
            }
            RpcMessage::AppendEntries(args) => {
                let mut success = false;
                let mut match_index = 0;

                if args.term >= self.current_term {
                    self.current_term = args.term;
                    self.save_term();

                    self.role = Role::Follower;
                    self.current_leader = Some(args.leader_id);
                    self.current_leader_address = Some(args.leader_address.clone());
                    self.last_election_time = Instant::now();

                    // Check if prev_log_index is in snapshot
                    if args.prev_log_index < self.last_included_index {
                        // Follower is ahead, reject
                        success = false;
                    } else if args.prev_log_index == self.last_included_index {
                        // Prev log is the snapshot point
                        success = args.prev_log_term == self.last_included_term;
                        if success {
                            // Append all entries
                            for (i, entry) in args.entries.iter().enumerate() {
                                let absolute_index = args.prev_log_index + 1 + i;
                                let log_index = absolute_index - self.last_included_index;

                                if log_index < self.log.len() {
                                    if self.log[log_index].term != entry.term {
                                        self.log.truncate(log_index);
                                        self.delete_log_entries_from(absolute_index);

                                        self.log.push(entry.clone());
                                        self.save_log_entry(absolute_index, entry);
                                    }
                                } else {
                                    self.log.push(entry.clone());
                                    self.save_log_entry(absolute_index, entry);
                                }
                            }
                            match_index = args.prev_log_index + args.entries.len();
                        }
                    } else {
                        // Normal case
                        let prev_log_index_local = args.prev_log_index - self.last_included_index;
                        if prev_log_index_local < self.log.len()
                            && self.log[prev_log_index_local].term == args.prev_log_term
                        {
                            success = true;

                            for (i, entry) in args.entries.iter().enumerate() {
                                let absolute_index = args.prev_log_index + 1 + i;
                                let log_index = absolute_index - self.last_included_index;

                                if log_index < self.log.len() {
                                    if self.log[log_index].term != entry.term {
                                        self.log.truncate(log_index);
                                        self.delete_log_entries_from(absolute_index);

                                        self.log.push(entry.clone());
                                        self.save_log_entry(absolute_index, entry);
                                    }
                                } else {
                                    self.log.push(entry.clone());
                                    self.save_log_entry(absolute_index, entry);
                                }
                            }

                            match_index = args.prev_log_index + args.entries.len();
                        }
                    }

                    if success && args.leader_commit > self.commit_index {
                        self.commit_index = args
                            .leader_commit
                            .min(self.log.len() - 1 + self.last_included_index);
                        self.apply_logs();
                    }
                }
                RpcMessage::AppendEntriesResponse(AppendEntriesReply {
                    term: self.current_term,
                    success,
                    match_index,
                    peer_id: self.id,
                })
            }
            RpcMessage::AppendEntriesResponse(reply) => {
                // Leader receives this - could update next_index/match_index here
                RpcMessage::AppendEntriesResponse(reply)
            }
            RpcMessage::InstallSnapshot(args) => {
                if args.term >= self.current_term {
                    self.current_term = args.term;
                    self.save_term();

                    self.role = Role::Follower;
                    self.current_leader = Some(args.leader_id);
                    self.last_election_time = Instant::now();

                    // Only install if snapshot is newer than our current state
                    if args.last_included_index > self.last_included_index {
                        let snapshot = Snapshot {
                            last_included_index: args.last_included_index,
                            last_included_term: args.last_included_term,
                            data: args.data,
                        };
                        self.install_snapshot(snapshot);
                    }
                }
                RpcMessage::InstallSnapshotResponse(InstallSnapshotReply {
                    term: self.current_term,
                    last_included_index: self.last_included_index,
                    peer_id: self.id,
                })
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
                        // Update next_index and match_index
                        self.next_index[peer_idx] = reply.last_included_index + 1;
                        self.match_index[peer_idx] = reply.last_included_index;
                    }
                } else if reply.term > self.current_term {
                    self.current_term = reply.term;
                    self.save_term();
                    self.role = Role::Follower;
                    self.voted_for = None;
                    self.save_vote();
                    self.current_leader = None;
                    self.current_leader_address = None;
                }
                RpcMessage::InstallSnapshotResponse(reply)
            }
        }
    }

    fn apply_logs(&mut self) {
        while self.commit_index > self.last_applied {
            self.last_applied += 1;
            let log_index = self.last_applied - self.last_included_index;
            if log_index < self.log.len() {
                let command = &self.log[log_index].command;
                self.apply_command(command);
            } else {
                eprintln!("Warning: log_index {} >= log.len() {} during apply_logs. last_applied={}, last_included_index={}",
                    log_index, self.log.len(), self.last_applied, self.last_included_index);
            }
        }
    }

    fn apply_command(&self, command: &Command) {
        let mut state = self.app_state.lock().unwrap();
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
                                println!(
                                    "Renamed file from {} to {} (same-shard)",
                                    source_path, dest_path
                                );
                            } else {
                                eprintln!("RenameFile: source file {} not found", source_path);
                            }
                        }
                        MasterCommand::CreateTransactionRecord { record } => {
                            master_state
                                .transaction_records
                                .insert(record.tx_id.clone(), record.clone());
                            println!(
                                "Created transaction record: tx_id={}, state={:?}",
                                record.tx_id, record.state
                            );
                        }
                        MasterCommand::UpdateTransactionState { tx_id, new_state } => {
                            if let Some(record) = master_state.transaction_records.get_mut(tx_id) {
                                let old_state = record.state.clone();
                                record.state = new_state.clone();
                                println!(
                                    "Updated transaction {} state: {:?} -> {:?}",
                                    tx_id, old_state, new_state
                                );
                            } else {
                                eprintln!(
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
                                        println!("Transaction {}: deleted file {}", tx_id, path);
                                    } else {
                                        eprintln!(
                                            "Transaction {}: file {} not found for deletion",
                                            tx_id, path
                                        );
                                    }
                                }
                                crate::master::TxOpType::Create { path, metadata } => {
                                    master_state.files.insert(path.clone(), metadata.clone());
                                    println!("Transaction {}: created file {}", tx_id, path);
                                }
                            }
                        }
                        MasterCommand::DeleteTransactionRecord { tx_id } => {
                            if master_state.transaction_records.remove(tx_id).is_some() {
                                println!("Deleted transaction record: tx_id={}", tx_id);
                            } else {
                                eprintln!(
                                    "DeleteTransactionRecord: transaction {} not found",
                                    tx_id
                                );
                            }
                        }
                    }
                } else {
                    eprintln!("Error: Received MasterCommand but state is not MasterState");
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
                    }
                } else {
                    eprintln!("Error: Received ConfigCommand but state is not ConfigState");
                }
            }
            Command::NoOp => {}
        }
    }
}
