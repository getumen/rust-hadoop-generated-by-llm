use crate::dfs::chunk_server_service_server::ChunkServerService;
use crate::dfs::{ReadBlockRequest, ReadBlockResponse, WriteBlockRequest, WriteBlockResponse};
use std::fs;
use std::io::{Read, Write};
use std::path::PathBuf;
use tonic::{Request, Response, Status};

#[derive(Debug)]
pub struct MyChunkServer {
    storage_dir: PathBuf,
}

impl MyChunkServer {
    pub fn new(storage_dir: PathBuf) -> Self {
        fs::create_dir_all(&storage_dir).unwrap();
        MyChunkServer { storage_dir }
    }
}

#[tonic::async_trait]
impl ChunkServerService for MyChunkServer {
    async fn write_block(
        &self,
        request: Request<WriteBlockRequest>,
    ) -> Result<Response<WriteBlockResponse>, Status> {
        let req = request.into_inner();
        let path = self.storage_dir.join(&req.block_id);

        // Write block locally
        match fs::File::create(&path) {
            Ok(mut file) => {
                if let Err(e) = file.write_all(&req.data) {
                    return Ok(Response::new(WriteBlockResponse {
                        success: false,
                        error_message: e.to_string(),
                    }));
                }

                // If there are next servers in the pipeline, replicate to them
                if !req.next_servers.is_empty() {
                    let next_server = &req.next_servers[0];
                    let remaining_servers = req.next_servers[1..].to_vec();
                    
                    // Forward to next server in pipeline
                    let next_addr = format!("http://{}", next_server);
                    match crate::dfs::chunk_server_service_client::ChunkServerServiceClient::connect(next_addr).await {
                        Ok(client) => {
                            let mut client = client.max_decoding_message_size(100 * 1024 * 1024);
                            let replicate_req = crate::dfs::ReplicateBlockRequest {
                                block_id: req.block_id,
                                data: req.data,
                                next_servers: remaining_servers,
                            };
                            
                            if let Err(e) = client.replicate_block(replicate_req).await {
                                eprintln!("Failed to replicate to {}: {}", next_server, e);
                                // Continue even if replication fails
                            }
                        }
                        Err(e) => {
                            eprintln!("Failed to connect to {} for replication: {}", next_server, e);
                        }
                    }
                }

                Ok(Response::new(WriteBlockResponse {
                    success: true,
                    error_message: "".to_string(),
                }))
            }
            Err(e) => Ok(Response::new(WriteBlockResponse {
                success: false,
                error_message: e.to_string(),
            })),
        }
    }

    async fn read_block(
        &self,
        request: Request<ReadBlockRequest>,
    ) -> Result<Response<ReadBlockResponse>, Status> {
        let req = request.into_inner();
        let path = self.storage_dir.join(&req.block_id);

        match fs::File::open(&path) {
            Ok(mut file) => {
                let mut data = Vec::new();
                if let Err(e) = file.read_to_end(&mut data) {
                    return Err(Status::internal(e.to_string()));
                }
                Ok(Response::new(ReadBlockResponse { data }))
            }
            Err(_) => Err(Status::not_found("Block not found")),
        }
    }

    async fn replicate_block(
        &self,
        request: Request<crate::dfs::ReplicateBlockRequest>,
    ) -> Result<Response<crate::dfs::ReplicateBlockResponse>, Status> {
        let req = request.into_inner();
        let path = self.storage_dir.join(&req.block_id);

        // Write block locally
        match fs::File::create(&path) {
            Ok(mut file) => {
                if let Err(e) = file.write_all(&req.data) {
                    return Ok(Response::new(crate::dfs::ReplicateBlockResponse {
                        success: false,
                        error_message: e.to_string(),
                    }));
                }

                // If there are more servers in the pipeline, continue replication
                if !req.next_servers.is_empty() {
                    let next_server = &req.next_servers[0];
                    let remaining_servers = req.next_servers[1..].to_vec();
                    
                    let next_addr = format!("http://{}", next_server);
                    match crate::dfs::chunk_server_service_client::ChunkServerServiceClient::connect(next_addr).await {
                        Ok(client) => {
                            let mut client = client.max_decoding_message_size(100 * 1024 * 1024);
                            let replicate_req = crate::dfs::ReplicateBlockRequest {
                                block_id: req.block_id,
                                data: req.data,
                                next_servers: remaining_servers,
                            };
                            
                            if let Err(e) = client.replicate_block(replicate_req).await {
                                eprintln!("Failed to replicate to {}: {}", next_server, e);
                            }
                        }
                        Err(e) => {
                            eprintln!("Failed to connect to {} for replication: {}", next_server, e);
                        }
                    }
                }

                Ok(Response::new(crate::dfs::ReplicateBlockResponse {
                    success: true,
                    error_message: "".to_string(),
                }))
            }
            Err(e) => Ok(Response::new(crate::dfs::ReplicateBlockResponse {
                success: false,
                error_message: e.to_string(),
            })),
        }
    }
}
