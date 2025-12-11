use crate::raft_types::{Node, TypeConfig};
use openraft::error::InstallSnapshotError;
use openraft::error::NetworkError;
use openraft::error::RPCError;
use openraft::error::RaftError;
use openraft::network::RPCOption;
use openraft::raft::AppendEntriesRequest;
use openraft::raft::AppendEntriesResponse;
use openraft::raft::InstallSnapshotRequest;
use openraft::raft::InstallSnapshotResponse;
use openraft::raft::VoteRequest;
use openraft::raft::VoteResponse;

pub struct Network {}

impl openraft::RaftNetworkFactory<TypeConfig> for Network {
    type Network = NetworkConnection;

    async fn new_client(&mut self, _target: u64, node: &Node) -> Self::Network {
        NetworkConnection {
            target_node: node.clone(),
        }
    }
}

pub struct NetworkConnection {
    target_node: Node,
}

impl openraft::RaftNetwork<TypeConfig> for NetworkConnection {
    async fn append_entries(
        &mut self,
        _req: AppendEntriesRequest<TypeConfig>,
        _option: RPCOption,
    ) -> Result<AppendEntriesResponse<TypeConfig>, RPCError<TypeConfig, RaftError<TypeConfig>>>
    {
        let url = format!("http://{}/raft/append", self.target_node.addr);
        let client = reqwest::Client::new();
        let _resp = client
            .post(url)
            // .json(&req)
            .send()
            .await
            .map_err(|e| RPCError::Network(NetworkError::new(&e)))?;
        // let res: AppendEntriesResponse<TypeConfig> = resp
        //     .json()
        //     .await
        //     .map_err(|e| RPCError::Network(NetworkError::new(&e)))?;
        // Ok(res)
        todo!()
    }

    async fn install_snapshot(
        &mut self,
        _req: InstallSnapshotRequest<TypeConfig>,
        _option: RPCOption,
    ) -> Result<
        InstallSnapshotResponse<TypeConfig>,
        RPCError<TypeConfig, RaftError<TypeConfig, InstallSnapshotError>>,
    > {
        let url = format!("http://{}/raft/install_snapshot", self.target_node.addr);
        let client = reqwest::Client::new();
        let _resp = client
            .post(url)
            // .json(&req)
            .send()
            .await
            .map_err(|e| RPCError::Network(NetworkError::new(&e)))?;
        // let res: InstallSnapshotResponse<TypeConfig> = resp
        //     .json()
        //     .await
        //     .map_err(|e| RPCError::Network(NetworkError::new(&e)))?;
        // Ok(res)
        todo!()
    }

    async fn vote(
        &mut self,
        _req: VoteRequest<TypeConfig>,
        _option: RPCOption,
    ) -> Result<VoteResponse<TypeConfig>, RPCError<TypeConfig, RaftError<TypeConfig>>> {
        let url = format!("http://{}/raft/vote", self.target_node.addr);
        let client = reqwest::Client::new();
        let _resp = client
            .post(url)
            // .json(&req)
            .send()
            .await
            .map_err(|e| RPCError::Network(NetworkError::new(&e)))?;
        // let res: VoteResponse<TypeConfig> = resp
        //     .json()
        //     .await
        //     .map_err(|e| RPCError::Network(NetworkError::new(&e)))?;
        // Ok(res)
        todo!()
    }
}
