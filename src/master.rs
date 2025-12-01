use crate::dfs::master_service_server::MasterService;
use crate::dfs::{
    AllocateBlockRequest, AllocateBlockResponse, BlockInfo, CompleteFileRequest, CompleteFileResponse,
    CreateFileRequest, CreateFileResponse, FileMetadata, GetFileInfoRequest, GetFileInfoResponse,
    ListFilesRequest, ListFilesResponse, RegisterChunkServerRequest, RegisterChunkServerResponse,
};
use crate::simple_raft::{Command, Event};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;
use tonic::{Request, Response, Status};
use uuid::Uuid;

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct MasterState {
    pub files: HashMap<String, FileMetadata>,
    #[serde(skip)]
    pub chunk_servers: Vec<String>, // List of chunk server addresses (ephemeral)
}

#[derive(Debug)]
pub struct MyMaster {
    state: Arc<Mutex<MasterState>>,
    raft_tx: mpsc::Sender<Event>,
}

impl MyMaster {
    pub fn new(state: Arc<Mutex<MasterState>>, raft_tx: mpsc::Sender<Event>) -> Self {
        MyMaster {
            state,
            raft_tx,
        }
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
        if let Err(_) = self.raft_tx.send(Event::ClientRequest {
            command: Command::CreateFile { path: req.path },
            reply_tx: tx,
        }).await {
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
            
            let chunk_servers = state.chunk_servers.clone();
            if chunk_servers.is_empty() {
                return Err(Status::unavailable("No chunk servers available"));
            }
            (chunk_servers, Uuid::new_v4().to_string())
        };

        // Select chunk servers
        let num_replicas = std::cmp::min(REPLICATION_FACTOR, chunk_servers.len());
        let selected_servers: Vec<String> = chunk_servers.iter()
            .take(num_replicas)
            .cloned()
            .collect();

        let (tx, rx) = tokio::sync::oneshot::channel();
        if let Err(_) = self.raft_tx.send(Event::ClientRequest {
            command: Command::AllocateBlock { 
                path: req.path, 
                block_id: block_id.clone(), 
                locations: selected_servers.clone() 
            },
            reply_tx: tx,
        }).await {
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
            },
            Ok(Err(leader_opt)) => {
                let leader_hint = leader_opt.unwrap_or_default();

                
                Ok(Response::new(AllocateBlockResponse {
                    block: None,
                    chunk_server_addresses: vec![],
                    leader_hint,
                }))
            },
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
        
        // We can handle registration locally or via Raft.
        // For simplicity, let's handle it locally (ephemeral state).
        // Or via Raft to ensure all masters know about chunkservers?
        // If we handle locally, only the connected master knows.
        // But chunkservers connect to ALL masters in our current implementation.
        // So local registration is fine.
        
        let mut state = self.state.lock().unwrap();
        if !state.chunk_servers.contains(&req.address) {
             state.chunk_servers.push(req.address);
        }

        Ok(Response::new(RegisterChunkServerResponse { success: true }))
    }
}
