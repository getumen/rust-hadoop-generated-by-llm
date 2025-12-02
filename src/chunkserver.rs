use crate::dfs::chunk_server_service_server::ChunkServerService;
use crate::dfs::{ReadBlockRequest, ReadBlockResponse, WriteBlockRequest, WriteBlockResponse};
use std::fs;
use std::io::{Read, Write};
use std::path::PathBuf;
use tonic::{Request, Response, Status};

#[derive(Debug, Clone)]
pub struct MyChunkServer {
    storage_dir: PathBuf,
}

impl MyChunkServer {
    pub fn new(storage_dir: PathBuf) -> Self {
        fs::create_dir_all(&storage_dir).unwrap();
        MyChunkServer { storage_dir }
    }

    fn calculate_checksums(data: &[u8]) -> Vec<u32> {
        let mut checksums = Vec::new();
        for chunk in data.chunks(512) {
            let mut hasher = crc32fast::Hasher::new();
            hasher.update(chunk);
            checksums.push(hasher.finalize());
        }
        checksums
    }

    fn write_block_local(&self, block_id: &str, data: &[u8]) -> Result<(), std::io::Error> {
        let path = self.storage_dir.join(block_id);
        let meta_path = self.storage_dir.join(format!("{}.meta", block_id));

        // Write data
        let mut file = fs::File::create(&path)?;
        file.write_all(data)?;

        // Calculate and write checksums
        let checksums = Self::calculate_checksums(data);
        let mut meta_file = fs::File::create(&meta_path)?;
        for checksum in checksums {
            meta_file.write_all(&checksum.to_be_bytes())?;
        }

        Ok(())
    }

    fn verify_block(&self, block_id: &str, data: &[u8]) -> Result<(), String> {
        let meta_path = self.storage_dir.join(format!("{}.meta", block_id));
        if !meta_path.exists() {
            // If meta file is missing, we can't verify. 
            // For now, treat as error to enforce integrity.
            return Err("Checksum file missing".to_string());
        }
        
        let mut meta_file = fs::File::open(&meta_path).map_err(|e| e.to_string())?;
        let mut meta_data = Vec::new();
        meta_file.read_to_end(&mut meta_data).map_err(|e| e.to_string())?;
        
        let expected_checksums: Vec<u32> = meta_data
            .chunks_exact(4)
            .map(|chunk| {
                let bytes: [u8; 4] = chunk.try_into().map_err(|_| "Invalid checksum size".to_string())?;
                Ok(u32::from_be_bytes(bytes))
            })
            .collect::<Result<Vec<u32>, String>>()?;
            
        let actual_checksums = Self::calculate_checksums(data);
        
        if expected_checksums.len() != actual_checksums.len() {
             return Err("Checksum count mismatch".to_string());
        }
        
        for (i, (expected, actual)) in expected_checksums.iter().zip(actual_checksums.iter()).enumerate() {
            if expected != actual {
                return Err(format!("Checksum mismatch at chunk {}", i));
            }
        }
        
        Ok(())
    }
    pub async fn run_background_scrubber(storage_dir: PathBuf, interval: std::time::Duration) {
        let server = MyChunkServer { storage_dir: storage_dir.clone() };
        
        loop {
            tokio::time::sleep(interval).await;
            println!("Starting background block scrubber...");
            
            let server_clone = server.clone();
            let storage_dir_clone = storage_dir.clone();
            
            let result = tokio::task::spawn_blocking(move || {
                match fs::read_dir(&storage_dir_clone) {
                    Ok(entries) => {
                        for entry in entries.flatten() {
                            let path = entry.path();
                            // Skip meta files and directories
                            if path.is_dir() || path.extension().map_or(false, |ext| ext == "meta") {
                                continue;
                            }
                            
                            if let Some(block_id) = path.file_name().and_then(|n| n.to_str()) {
                                // Read block data
                                match fs::read(&path) {
                                    Ok(data) => {
                                        match server_clone.verify_block(block_id, &data) {
                                            Ok(_) => {},
                                            Err(e) => eprintln!("Scrubber detected corruption in block {}: {}", block_id, e),
                                        }
                                    }
                                    Err(e) => eprintln!("Failed to read block {}: {}", block_id, e),
                                }
                            }
                        }
                    }
                    Err(e) => eprintln!("Failed to read storage directory: {}", e),
                }
            }).await;

            if let Err(e) = result {
                eprintln!("Scrubber task failed: {}", e);
            }
            
            println!("Background block scrubber finished.");
        }
    }
}

#[tonic::async_trait]
impl ChunkServerService for MyChunkServer {
    async fn write_block(
        &self,
        request: Request<WriteBlockRequest>,
    ) -> Result<Response<WriteBlockResponse>, Status> {
        let req = request.into_inner();

        // Write block locally with checksums
        if let Err(e) = self.write_block_local(&req.block_id, &req.data) {
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
                
                // Verify checksums
                if let Err(e) = self.verify_block(&req.block_id, &data) {
                    eprintln!("Data corruption detected for block {}: {}", req.block_id, e);
                    return Err(Status::data_loss(format!("Data corruption detected: {}", e)));
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

        // Write block locally with checksums
        if let Err(e) = self.write_block_local(&req.block_id, &req.data) {
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_checksum_verification() {
        let dir = tempdir().unwrap();
        let server = MyChunkServer::new(dir.path().to_path_buf());
        let block_id = "test_block";
        let data = b"Hello, world! This is a test block for checksum verification.";

        // Test write and verify
        server.write_block_local(block_id, data).unwrap();
        server.verify_block(block_id, data).unwrap();

        // Test corruption
        let path = dir.path().join(block_id);
        let mut file = fs::OpenOptions::new().write(true).open(&path).unwrap();
        file.write_all(b"Corrupted").unwrap(); // Overwrite beginning

        // Read corrupted data
        let mut file = fs::File::open(&path).unwrap();
        let mut corrupted_data = Vec::new();
        file.read_to_end(&mut corrupted_data).unwrap();

        // Verify should fail
        assert!(server.verify_block(block_id, &corrupted_data).is_err());
    }
}
