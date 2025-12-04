use crate::dfs::master_service_server::MasterService;
use crate::dfs::{
    AllocateBlockRequest, AllocateBlockResponse, BlockInfo, CompleteFileRequest,
    CompleteFileResponse, CreateFileRequest, CreateFileResponse, FileMetadata,
    GetBlockLocationsRequest, GetBlockLocationsResponse, GetFileInfoRequest, GetFileInfoResponse,
    HeartbeatRequest, HeartbeatResponse, ListFilesRequest, ListFilesResponse,
    RegisterChunkServerRequest, RegisterChunkServerResponse,
};
use crate::simple_raft::{Command, Event};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;
use tonic::{Request, Response, Status};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChunkServerStatus {
    pub last_heartbeat: u64,
    pub used_space: u64,
    pub available_space: u64,
    pub chunk_count: u64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct MasterState {
    pub files: HashMap<String, FileMetadata>,
    #[serde(skip)]
    pub chunk_servers: HashMap<String, ChunkServerStatus>, // address -> status
}

#[derive(Debug)]
pub struct MyMaster {
    state: Arc<Mutex<MasterState>>,
    raft_tx: mpsc::Sender<Event>,
}

impl MyMaster {
    pub fn new(state: Arc<Mutex<MasterState>>, raft_tx: mpsc::Sender<Event>) -> Self {
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

                let mut state = state_clone.lock().unwrap();
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
                }
            }
        });

        MyMaster { state, raft_tx }
    }
}

#[tonic::async_trait]
impl MasterService for MyMaster {
    async fn get_file_info(
        &self,
        request: Request<GetFileInfoRequest>,
    ) -> Result<Response<GetFileInfoResponse>, Status> {
        let req = request.into_inner();
        let state = self.state.lock().unwrap();

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
    }

    async fn create_file(
        &self,
        request: Request<CreateFileRequest>,
    ) -> Result<Response<CreateFileResponse>, Status> {
        let req = request.into_inner();

        // Check if file exists (read optimization)
        {
            let state = self.state.lock().unwrap();
            if state.files.contains_key(&req.path) {
                return Ok(Response::new(CreateFileResponse {
                    success: false,
                    error_message: "File already exists".to_string(),
                    leader_hint: "".to_string(),
                }));
            }
        }

        let (tx, rx) = tokio::sync::oneshot::channel();
        if self
            .raft_tx
            .send(Event::ClientRequest {
                command: Command::CreateFile { path: req.path },
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
            let state = self.state.lock().unwrap();
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

            let chunk_servers: Vec<String> = candidates.into_iter().map(|(addr, _)| addr).collect();
            (chunk_servers, Uuid::new_v4().to_string())
        };

        // Select chunk servers
        let num_replicas = std::cmp::min(REPLICATION_FACTOR, chunk_servers.len());
        let selected_servers: Vec<String> =
            chunk_servers.iter().take(num_replicas).cloned().collect();

        let (tx, rx) = tokio::sync::oneshot::channel();
        if self
            .raft_tx
            .send(Event::ClientRequest {
                command: Command::AllocateBlock {
                    path: req.path,
                    block_id: block_id.clone(),
                    locations: selected_servers.clone(),
                },
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
        let state = self.state.lock().unwrap();
        let files: Vec<String> = state.files.keys().cloned().collect();
        Ok(Response::new(ListFilesResponse { files }))
    }

    async fn register_chunk_server(
        &self,
        request: Request<RegisterChunkServerRequest>,
    ) -> Result<Response<RegisterChunkServerResponse>, Status> {
        let req = request.into_inner();

        let mut state = self.state.lock().unwrap();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;

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

        Ok(Response::new(RegisterChunkServerResponse { success: true }))
    }

    async fn heartbeat(
        &self,
        request: Request<HeartbeatRequest>,
    ) -> Result<Response<HeartbeatResponse>, Status> {
        let req = request.into_inner();
        let mut state = self.state.lock().unwrap();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;

        state.chunk_servers.insert(
            req.chunk_server_address,
            ChunkServerStatus {
                last_heartbeat: now,
                used_space: req.used_space,
                available_space: req.available_space,
                chunk_count: req.chunk_count,
            },
        );
        Ok(Response::new(HeartbeatResponse { success: true }))
    }

    async fn get_block_locations(
        &self,
        request: Request<GetBlockLocationsRequest>,
    ) -> Result<Response<GetBlockLocationsResponse>, Status> {
        let req = request.into_inner();
        let state = self.state.lock().unwrap();

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

        // Block not found
        Ok(Response::new(GetBlockLocationsResponse {
            locations: vec![],
            found: false,
        }))
    }
}
