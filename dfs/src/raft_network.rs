use crate::raft_types::{Node, NodeId, TypeConfig};
use async_trait::async_trait;
use openraft::error::InstallSnapshotError;
use openraft::error::NetworkError;
use openraft::error::RPCError;
use openraft::error::RaftError;
use openraft::error::RemoteError;
use openraft::raft::AppendEntriesRequest;
use openraft::raft::AppendEntriesResponse;
use openraft::raft::InstallSnapshotRequest;
use openraft::raft::InstallSnapshotResponse;
use openraft::raft::VoteRequest;
use openraft::raft::VoteResponse;

pub struct Network {}

#[async_trait]
impl openraft::RaftNetworkFactory<TypeConfig> for Network {
    type Network = NetworkConnection;

    async fn new_client(&self, target: NodeId, node: &Node) -> Self::Network {
        NetworkConnection {
            target,
            target_node: node.clone(),
        }
    }
}

pub struct NetworkConnection {
    target: NodeId,
    target_node: Node,
}

#[async_trait]
impl openraft::RaftNetwork<TypeConfig> for NetworkConnection {
    async fn send_append_entries(
        &mut self,
        req: AppendEntriesRequest<TypeConfig>,
    ) -> Result<AppendEntriesResponse<NodeId>, RPCError<NodeId, NodeId, RaftError<NodeId>>> {
        let url = format!("http://{}/raft/append", self.target_node.addr);
        let client = reqwest::Client::new();
        let resp = client
            .post(url)
            .json(&req)
            .send()
            .await
            .map_err(|e| RPCError::Network(NetworkError::new(&e)))?;
        let res: AppendEntriesResponse<NodeId> = resp
            .json()
            .await
            .map_err(|e| RPCError::Network(NetworkError::new(&e)))?;
        Ok(res)
    }

    async fn send_install_snapshot(
        &mut self,
        req: InstallSnapshotRequest<TypeConfig>,
    ) -> Result<
        InstallSnapshotResponse<NodeId>,
        RPCError<NodeId, NodeId, RaftError<NodeId, InstallSnapshotError>>,
    > {
        let url = format!("http://{}/raft/snapshot", self.target_node.addr);
        let client = reqwest::Client::new();
        let resp = client
            .post(url)
            .json(&req)
            .send()
            .await
            .map_err(|e| RPCError::Network(NetworkError::new(&e)))?;
        let res: InstallSnapshotResponse<NodeId> = resp
            .json()
            .await
            .map_err(|e| RPCError::Network(NetworkError::new(&e)))?;
        Ok(res)
    }

    async fn send_vote(
        &mut self,
        req: VoteRequest<NodeId>,
    ) -> Result<VoteResponse<NodeId>, RPCError<NodeId, NodeId, RaftError<NodeId>>> {
        let url = format!("http://{}/raft/vote", self.target_node.addr);
        let client = reqwest::Client::new();
        let resp = client
            .post(url)
            .json(&req)
            .send()
            .await
            .map_err(|e| RPCError::Network(NetworkError::new(&e)))?;
        let res: VoteResponse<NodeId> = resp
            .json()
            .await
            .map_err(|e| RPCError::Network(NetworkError::new(&e)))?;
        Ok(res)
    }
}
