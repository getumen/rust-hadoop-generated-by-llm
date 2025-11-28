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

        match fs::File::create(&path) {
            Ok(mut file) => {
                if let Err(e) = file.write_all(&req.data) {
                    return Ok(Response::new(WriteBlockResponse {
                        success: false,
                        error_message: e.to_string(),
                    }));
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
}
