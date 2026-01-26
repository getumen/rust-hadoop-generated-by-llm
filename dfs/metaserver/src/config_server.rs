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
        if let AppState::Config(ref config_state) = *state_lock {
            let mut shards = HashMap::new();
            for shard_id in config_state.shard_map.get_all_shards() {
                if let Some(peers) = config_state.shard_map.get_shard_peers(&shard_id) {
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

        if self
            .raft_tx
            .send(Event::ClientRequest {
                command: Command::Config(ConfigCommand::AddShard {
                    shard_id: req.shard_id,
                    peers: req.peers,
                }),
                reply_tx: tx,
            })
            .await
            .is_err()
        {
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

        if self
            .raft_tx
            .send(Event::ClientRequest {
                command: Command::Config(ConfigCommand::RemoveShard {
                    shard_id: req.shard_id,
                }),
                reply_tx: tx,
            })
            .await
            .is_err()
        {
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

    async fn split_shard(
        &self,
        request: Request<crate::dfs::SplitShardRequest>,
    ) -> Result<Response<crate::dfs::SplitShardResponse>, Status> {
        let mut req = request.into_inner();
        let (tx, rx) = tokio::sync::oneshot::channel();

        // Automatic Peer Allocation
        if req.new_shard_peers.is_empty() {
            let state_lock = self.state.lock().unwrap();
            if let AppState::Config(ref config_state) = *state_lock {
                // Heuristic: Pick up to 3 healthiest (most recent heartbeat) masters
                let mut available: Vec<_> = config_state.masters.values().collect();
                available.sort_by_key(|m| std::cmp::Reverse(m.last_heartbeat));
                req.new_shard_peers = available
                    .iter()
                    .take(3)
                    .map(|m| m.address.clone())
                    .collect();
            }
        }

        if req.new_shard_peers.is_empty() {
            return Ok(Response::new(crate::dfs::SplitShardResponse {
                success: false,
                error_message: "No available master nodes for new shard".to_string(),
                leader_hint: "".to_string(),
                new_shard_peers: vec![],
            }));
        }

        if self
            .raft_tx
            .send(Event::ClientRequest {
                command: Command::Config(ConfigCommand::SplitShard {
                    shard_id: req.shard_id,
                    split_key: req.split_key,
                    new_shard_id: req.new_shard_id,
                    new_shard_peers: req.new_shard_peers.clone(),
                }),
                reply_tx: tx,
            })
            .await
            .is_err()
        {
            return Err(Status::internal("Raft channel closed"));
        }

        match rx.await {
            Ok(Ok(())) => Ok(Response::new(crate::dfs::SplitShardResponse {
                success: true,
                error_message: "".to_string(),
                leader_hint: "".to_string(),
                new_shard_peers: req.new_shard_peers,
            })),
            Ok(Err(leader_opt)) => Ok(Response::new(crate::dfs::SplitShardResponse {
                success: false,
                error_message: "Not Leader".to_string(),
                leader_hint: leader_opt.unwrap_or_default(),
                new_shard_peers: vec![],
            })),
            Err(_) => Err(Status::internal("Raft response error")),
        }
    }

    async fn merge_shard(
        &self,
        request: Request<crate::dfs::MergeShardRequest>,
    ) -> Result<Response<crate::dfs::MergeShardResponse>, Status> {
        let req = request.into_inner();
        let (tx, rx) = tokio::sync::oneshot::channel();

        if self
            .raft_tx
            .send(Event::ClientRequest {
                command: Command::Config(ConfigCommand::MergeShard {
                    victim_shard_id: req.victim_shard_id,
                    retained_shard_id: req.retained_shard_id,
                }),
                reply_tx: tx,
            })
            .await
            .is_err()
        {
            return Err(Status::internal("Raft channel closed"));
        }

        match rx.await {
            Ok(Ok(())) => Ok(Response::new(crate::dfs::MergeShardResponse {
                success: true,
                error_message: "".to_string(),
                leader_hint: "".to_string(),
            })),
            Ok(Err(leader_opt)) => Ok(Response::new(crate::dfs::MergeShardResponse {
                success: false,
                error_message: "Not Leader".to_string(),
                leader_hint: leader_opt.unwrap_or_default(),
            })),
            Err(_) => Err(Status::internal("Raft response error")),
        }
    }

    async fn rebalance_shard(
        &self,
        request: Request<crate::dfs::RebalanceShardRequest>,
    ) -> Result<Response<crate::dfs::RebalanceShardResponse>, Status> {
        let req = request.into_inner();
        let (tx, rx) = tokio::sync::oneshot::channel();

        if self
            .raft_tx
            .send(Event::ClientRequest {
                command: Command::Config(ConfigCommand::RebalanceShard {
                    old_key: req.old_key,
                    new_key: req.new_key,
                }),
                reply_tx: tx,
            })
            .await
            .is_err()
        {
            return Err(Status::internal("Raft channel closed"));
        }

        match rx.await {
            Ok(Ok(())) => Ok(Response::new(crate::dfs::RebalanceShardResponse {
                success: true,
                error_message: "".to_string(),
                leader_hint: "".to_string(),
            })),
            Ok(Err(leader_opt)) => Ok(Response::new(crate::dfs::RebalanceShardResponse {
                success: false,
                error_message: "Not Leader".to_string(),
                leader_hint: leader_opt.unwrap_or_default(),
            })),
            Err(_) => Err(Status::internal("Raft response error")),
        }
    }

    async fn register_master(
        &self,
        request: Request<crate::dfs::RegisterMasterRequest>,
    ) -> Result<Response<crate::dfs::RegisterMasterResponse>, Status> {
        let req = request.into_inner();
        let (tx, rx) = tokio::sync::oneshot::channel();

        if self
            .raft_tx
            .send(Event::ClientRequest {
                command: Command::Config(ConfigCommand::RegisterMaster {
                    address: req.address,
                    shard_id: req.shard_id,
                }),
                reply_tx: tx,
            })
            .await
            .is_err()
        {
            return Err(Status::internal("Raft channel closed"));
        }

        match rx.await {
            Ok(Ok(())) => Ok(Response::new(crate::dfs::RegisterMasterResponse {
                success: true,
            })),
            Ok(Err(_)) => Ok(Response::new(crate::dfs::RegisterMasterResponse {
                success: false,
            })),
            Err(_) => Err(Status::internal("Raft response error")),
        }
    }

    async fn shard_heartbeat(
        &self,
        request: Request<crate::dfs::ShardHeartbeatRequest>,
    ) -> Result<Response<crate::dfs::ShardHeartbeatResponse>, Status> {
        let req = request.into_inner();
        let (tx, rx) = tokio::sync::oneshot::channel();

        if self
            .raft_tx
            .send(Event::ClientRequest {
                command: Command::Config(ConfigCommand::ShardHeartbeat {
                    address: req.address,
                    rps_per_prefix: req.rps_per_prefix,
                }),
                reply_tx: tx,
            })
            .await
            .is_err()
        {
            return Err(Status::internal("Raft channel closed"));
        }

        match rx.await {
            Ok(Ok(())) => Ok(Response::new(crate::dfs::ShardHeartbeatResponse {
                success: true,
            })),
            Ok(Err(_)) => Ok(Response::new(crate::dfs::ShardHeartbeatResponse {
                success: false,
            })),
            Err(_) => Err(Status::internal("Raft response error")),
        }
    }
}
