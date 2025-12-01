use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;
use tokio::time::{Duration, Instant};
use rand::Rng;
use rocksdb::DB;
use crate::master::MasterState;

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
    CreateFile { path: String },
    AllocateBlock { path: String, block_id: String, locations: Vec<String> },
    RegisterChunkServer { address: String },
    NoOp,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum RpcMessage {
    RequestVote(RequestVoteArgs),
    RequestVoteResponse(RequestVoteReply),
    AppendEntries(AppendEntriesArgs),
    AppendEntriesResponse(AppendEntriesReply),
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
    
    pub election_timeout: Duration,
    pub last_election_time: Instant,
    
    pub inbox: mpsc::Receiver<Event>,
    pub self_tx: mpsc::Sender<Event>,
    pub app_state: Arc<Mutex<MasterState>>,
    pub votes_received: usize,
    
    pub db: Arc<DB>, // RocksDB instance
}

impl RaftNode {
    pub fn new(
        id: usize,
        peers: Vec<String>,
        client_address: String,
        storage_dir: String,
        app_state: Arc<Mutex<MasterState>>,
        inbox: mpsc::Receiver<Event>,
        self_tx: mpsc::Sender<Event>,
    ) -> Self {
        let path = format!("{}/raft_node_{}", storage_dir, id);
        let db = DB::open_default(&path).expect("Failed to open RocksDB");
        let db = Arc::new(db);

        let (current_term, voted_for, log) = Self::load_state(&db);
        
        RaftNode {
            id,
            peers,
            client_address,
            role: Role::Follower,
            current_term,
            voted_for,
            log,
            commit_index: 0,
            last_applied: 0,
            next_index: vec![],
            match_index: vec![],
            current_leader: None,
            current_leader_address: None,
            election_timeout: Duration::from_millis(rand::thread_rng().gen_range(1500..3000)),
            last_election_time: Instant::now(),
            inbox,
            self_tx,
            app_state,
            votes_received: 0,
            db,
        }
    }

    fn load_state(db: &DB) -> (u64, Option<usize>, Vec<LogEntry>) {
        let term = match db.get(b"term") {
            Ok(Some(val)) => u64::from_be_bytes(val.try_into().unwrap()),
            _ => 0,
        };
        let voted_for = match db.get(b"vote") {
            Ok(Some(val)) => Some(usize::from_be_bytes(val.try_into().unwrap())),
            _ => None,
        };
        
        let mut log = vec![LogEntry { term: 0, command: Command::NoOp }];
        let mut index = 1;
        loop {
            let key = format!("log:{}", index);
            match db.get(key.as_bytes()) {
                Ok(Some(val)) => {
                    let entry: LogEntry = serde_json::from_slice(&val).unwrap();
                    log.push(entry);
                    index += 1;
                }
                _ => break,
            }
        }
        
        (term, voted_for, log)
    }

    fn save_term(&self) {
        self.db.put(b"term", self.current_term.to_be_bytes()).unwrap();
    }

    fn save_vote(&self) {
        if let Some(vote) = self.voted_for {
            self.db.put(b"vote", vote.to_be_bytes()).unwrap();
        } else {
            self.db.delete(b"vote").unwrap();
        }
    }

    fn save_log_entry(&self, index: usize, entry: &LogEntry) {
        let key = format!("log:{}", index);
        let val = serde_json::to_vec(entry).unwrap();
        self.db.put(key.as_bytes(), val).unwrap();
    }

    fn delete_log_entries_from(&self, start_index: usize) {
        let mut index = start_index;
        loop {
            let key = format!("log:{}", index);
            if self.db.get(key.as_bytes()).unwrap().is_none() {
                break;
            }
            self.db.delete(key.as_bytes()).unwrap();
            index += 1;
        }
    }

    pub async fn run(&mut self) {
        let tick_rate = Duration::from_millis(100);
        let mut tick_interval = tokio::time::interval(tick_rate);

        self.next_index = vec![1; self.peers.len()];
        self.match_index = vec![0; self.peers.len()];

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
    }

    async fn start_election(&mut self) {
        self.role = Role::Candidate;
        self.current_term += 1;
        self.save_term(); // Persist term
        
        self.voted_for = Some(self.id);
        self.save_vote(); // Persist vote
        
        self.votes_received = 1;
        self.last_election_time = Instant::now();
        self.election_timeout = Duration::from_millis(rand::thread_rng().gen_range(1500..3000));
        
        println!("Node {} starting election for term {}", self.id, self.current_term);

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
            
            tokio::spawn(async move {
                let client = reqwest::Client::new();
                match client.post(url).json(&args).send().await {
                    Ok(resp) => {
                        if let Ok(reply) = resp.json::<RequestVoteReply>().await {
                            let _ = tx.send(Event::Rpc { 
                                msg: RpcMessage::RequestVoteResponse(reply), 
                                reply_tx: None 
                            }).await;
                        }
                    }
                    Err(_) => {} // Silently ignore network errors
                }
            });
        }
    }

    fn become_leader(&mut self) {
        println!("Node {} becoming LEADER for term {}", self.id, self.current_term);
        self.role = Role::Leader;
        self.current_leader = Some(self.id);
        self.current_leader_address = Some(self.client_address.clone());
        self.next_index = vec![self.log.len(); self.peers.len()];
        self.match_index = vec![0; self.peers.len()];
    }

    async fn send_heartbeats(&mut self) {
        for (i, peer) in self.peers.iter().enumerate() {
            let prev_log_index = self.next_index[i] - 1;
            let prev_log_term = self.log[prev_log_index].term;
            
            let entries = if self.log.len() > self.next_index[i] {
                self.log[self.next_index[i]..].to_vec()
            } else {
                vec![]
            };

            let args = AppendEntriesArgs {
                term: self.current_term,
                leader_id: self.id,
                prev_log_index,
                prev_log_term,
                entries,
                leader_commit: self.commit_index,
                leader_address: self.client_address.clone(),
            };

            let url = format!("{}/raft/append", peer);
            let tx = self.self_tx.clone();
            
            tokio::spawn(async move {
                let client = reqwest::Client::new();
                match client.post(url).json(&args).send().await {
                    Ok(resp) => {
                        if let Ok(reply) = resp.json::<AppendEntriesReply>().await {
                            let _ = tx.send(Event::Rpc {
                                msg: RpcMessage::AppendEntriesResponse(reply),
                                reply_tx: None
                            }).await;
                        }
                    }
                    Err(_) => {}
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
                if self.role != Role::Leader {
                    let _ = reply_tx.send(Err(self.current_leader_address.clone()));
                    return;
                }
                
                let entry = LogEntry {
                    term: self.current_term,
                    command,
                };
                self.log.push(entry.clone());
                self.save_log_entry(self.log.len() - 1, &entry); // Persist log entry
                
                self.commit_index = self.log.len() - 1;
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
                let mut vote_granted = false;
                if args.term >= self.current_term {
                    if args.term > self.current_term {
                        self.current_term = args.term;
                        self.save_term(); // Persist term
                        
                        self.role = Role::Follower;
                        self.voted_for = None;
                        self.save_vote(); // Persist vote (None)
                        
                        self.current_leader = None;
                        self.current_leader_address = None;
                    }
                    
                    if (self.voted_for.is_none() || self.voted_for == Some(args.candidate_id))
                        && args.last_log_index >= self.log.len() - 1 
                    {
                        self.voted_for = Some(args.candidate_id);
                        self.save_vote(); // Persist vote
                        
                        self.last_election_time = Instant::now();
                        vote_granted = true;
                    }
                }
                RpcMessage::RequestVoteResponse(RequestVoteReply {
                    term: self.current_term,
                    vote_granted,
                })
            }
            RpcMessage::RequestVoteResponse(reply) => {
                if self.role == Role::Candidate && self.current_term == reply.term && reply.vote_granted {
                    self.votes_received += 1;
                    let total_nodes = self.peers.len() + 1;
                    if self.votes_received > total_nodes / 2 {
                        self.become_leader();
                    }
                } else if reply.term > self.current_term {
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
                    self.save_term(); // Persist term
                    
                    self.role = Role::Follower;
                    self.current_leader = Some(args.leader_id);
                    self.current_leader_address = Some(args.leader_address.clone());
                    self.last_election_time = Instant::now();
                    
                    if args.prev_log_index < self.log.len() {
                        if self.log[args.prev_log_index].term == args.prev_log_term {
                            success = true;
                            
                            let mut log_insert_index = args.prev_log_index + 1;
                            for entry in args.entries {
                                if log_insert_index < self.log.len() {
                                    if self.log[log_insert_index].term != entry.term {
                                        self.log.truncate(log_insert_index);
                                        self.delete_log_entries_from(log_insert_index); // Persist truncate
                                        
                                        self.log.push(entry.clone());
                                        self.save_log_entry(log_insert_index, &entry); // Persist new entry
                                    }
                                } else {
                                    self.log.push(entry.clone());
                                    self.save_log_entry(log_insert_index, &entry); // Persist new entry
                                }
                                log_insert_index += 1;
                            }
                            
                            match_index = self.log.len() - 1;

                            if args.leader_commit > self.commit_index {
                                self.commit_index = args.leader_commit.min(self.log.len() - 1);
                                self.apply_logs();
                            }
                        }
                    }
                }
                RpcMessage::AppendEntriesResponse(AppendEntriesReply {
                    term: self.current_term,
                    success,
                    match_index,
                })
            }
            RpcMessage::AppendEntriesResponse(reply) => {
                // Leader receives this - could update next_index/match_index here
                RpcMessage::AppendEntriesResponse(reply)
            }
        }
    }

    fn apply_logs(&mut self) {
        while self.commit_index > self.last_applied {
            self.last_applied += 1;
            let command = &self.log[self.last_applied].command;
            self.apply_command(command);
        }
    }

    fn apply_command(&self, command: &Command) {
        let mut state = self.app_state.lock().unwrap();
        match command {
            Command::CreateFile { path } => {
                state.files.insert(path.clone(), crate::dfs::FileMetadata {
                    path: path.clone(),
                    size: 0,
                    blocks: vec![],
                });
            }
            Command::AllocateBlock { path, block_id, locations } => {
                if let Some(meta) = state.files.get_mut(path) {
                    meta.blocks.push(crate::dfs::BlockInfo {
                        block_id: block_id.clone(),
                        size: 0,
                        locations: locations.clone(),
                    });
                }
            }
            Command::RegisterChunkServer { address: _ } => {
                // ChunkServer registration is handled locally, not via Raft
            }
            Command::NoOp => {}
        }
    }
}
