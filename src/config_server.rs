use crate::dfs::config_service_server::ConfigService;
use crate::dfs::{
    AddShardRequest, AddShardResponse, FetchShardMapRequest, FetchShardMapResponse,
    RemoveShardRequest, RemoveShardResponse, ShardPeers,
};
use crate::simple_raft::{AppState, Command, ConfigCommand, Event};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;
use tonic::{Request, Response, Status};

#[derive(Debug)]
pub struct MyConfigServer {
    state: Arc<Mutex<AppState>>,
    raft_tx: mpsc::Sender<Event>,
}

impl MyConfigServer {
    pub fn new(state: Arc<Mutex<AppState>>, raft_tx: mpsc::Sender<Event>) -> Self {
        MyConfigServer { state, raft_tx }
    }
}

#[tonic::async_trait]
impl ConfigService for MyConfigServer {
    async fn fetch_shard_map(
        &self,
        _request: Request<FetchShardMapRequest>,
    ) -> Result<Response<FetchShardMapResponse>, Status> {
        let state_lock = self.state.lock().unwrap();
        if let AppState::Config(ref shard_map) = *state_lock {
            let mut shards = HashMap::new();
            for shard_id in shard_map.get_all_shards() {
                if let Some(peers) = shard_map.get_shard_peers(&shard_id) {
                    shards.insert(shard_id, ShardPeers { peers });
                }
            }
            Ok(Response::new(FetchShardMapResponse { shards }))
        } else {
            Err(Status::internal("Wrong state type"))
        }
    }

    async fn add_shard(
        &self,
        request: Request<AddShardRequest>,
    ) -> Result<Response<AddShardResponse>, Status> {
        let req = request.into_inner();
        let (tx, rx) = tokio::sync::oneshot::channel();
        
        if self.raft_tx.send(Event::ClientRequest {
            command: Command::Config(ConfigCommand::AddShard {
                shard_id: req.shard_id,
                peers: req.peers,
            }),
            reply_tx: tx,
        }).await.is_err() {
            return Err(Status::internal("Raft channel closed"));
        }

        match rx.await {
            Ok(Ok(())) => Ok(Response::new(AddShardResponse {
                success: true,
                error_message: "".to_string(),
                leader_hint: "".to_string(),
            })),
            Ok(Err(leader_opt)) => Ok(Response::new(AddShardResponse {
                success: false,
                error_message: "Not Leader".to_string(),
                leader_hint: leader_opt.unwrap_or_default(),
            })),
            Err(_) => Err(Status::internal("Raft response error")),
        }
    }

    async fn remove_shard(
        &self,
        request: Request<RemoveShardRequest>,
    ) -> Result<Response<RemoveShardResponse>, Status> {
        let req = request.into_inner();
        let (tx, rx) = tokio::sync::oneshot::channel();

        if self.raft_tx.send(Event::ClientRequest {
            command: Command::Config(ConfigCommand::RemoveShard {
                shard_id: req.shard_id,
            }),
            reply_tx: tx,
        }).await.is_err() {
            return Err(Status::internal("Raft channel closed"));
        }

        match rx.await {
            Ok(Ok(())) => Ok(Response::new(RemoveShardResponse {
                success: true,
                error_message: "".to_string(),
                leader_hint: "".to_string(),
            })),
            Ok(Err(leader_opt)) => Ok(Response::new(RemoveShardResponse {
                success: false,
                error_message: "Not Leader".to_string(),
                leader_hint: leader_opt.unwrap_or_default(),
            })),
            Err(_) => Err(Status::internal("Raft response error")),
        }
    }
}
