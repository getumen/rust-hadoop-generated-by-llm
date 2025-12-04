pub mod dfs {
    tonic::include_proto!("dfs");
}

pub mod chunkserver;
pub mod master;
pub mod sharding;
pub mod simple_raft;
