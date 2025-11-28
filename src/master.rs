use crate::dfs::master_service_server::MasterService;
use crate::dfs::{
    AllocateBlockRequest, AllocateBlockResponse, BlockInfo, CompleteFileRequest, CompleteFileResponse,
    CreateFileRequest, CreateFileResponse, FileMetadata, GetFileInfoRequest, GetFileInfoResponse,
    ListFilesRequest, ListFilesResponse, RegisterChunkServerRequest, RegisterChunkServerResponse,
};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tonic::{Request, Response, Status};
use uuid::Uuid;

#[derive(Debug, Default)]
pub struct MasterState {
    files: HashMap<String, FileMetadata>,
    chunk_servers: Vec<String>, // List of chunk server addresses
}

#[derive(Debug, Default)]
pub struct MyMaster {
    state: Arc<Mutex<MasterState>>,
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
        let mut state = self.state.lock().unwrap();

        if state.files.contains_key(&req.path) {
            return Ok(Response::new(CreateFileResponse {
                success: false,
                error_message: "File already exists".to_string(),
            }));
        }

        let metadata = FileMetadata {
            path: req.path.clone(),
            size: 0,
            blocks: vec![],
        };

        state.files.insert(req.path, metadata);

        Ok(Response::new(CreateFileResponse {
            success: true,
            error_message: "".to_string(),
        }))
    }

    async fn allocate_block(
        &self,
        request: Request<AllocateBlockRequest>,
    ) -> Result<Response<AllocateBlockResponse>, Status> {
        let req = request.into_inner();
        let mut state = self.state.lock().unwrap();

        // Replication factor (default: 3)
        const REPLICATION_FACTOR: usize = 3;

        // Clone chunk_servers first to avoid borrow conflict
        let chunk_servers = state.chunk_servers.clone();

        if chunk_servers.is_empty() {
            return Err(Status::unavailable("No chunk servers available"));
        }

        if let Some(metadata) = state.files.get_mut(&req.path) {
            let block_id = Uuid::new_v4().to_string();

            // Select chunk servers for replication (up to REPLICATION_FACTOR)
            let num_replicas = std::cmp::min(REPLICATION_FACTOR, chunk_servers.len());
            let selected_servers: Vec<String> = chunk_servers.iter()
                .take(num_replicas)
                .cloned()
                .collect();

            let block = BlockInfo {
                block_id: block_id.clone(),
                size: 0,
                locations: selected_servers.clone(),
            };
            metadata.blocks.push(block.clone());

            Ok(Response::new(AllocateBlockResponse {
                block: Some(block),
                chunk_server_addresses: selected_servers,
            }))
        } else {
            Err(Status::not_found("File not found"))
        }
    }

    async fn complete_file(
        &self,
        _request: Request<CompleteFileRequest>,
    ) -> Result<Response<CompleteFileResponse>, Status> {
        // In a real system, we might verify block replication here
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
        
        if !state.chunk_servers.contains(&req.address) {
             state.chunk_servers.push(req.address);
        }

        Ok(Response::new(RegisterChunkServerResponse { success: true }))
    }
}
