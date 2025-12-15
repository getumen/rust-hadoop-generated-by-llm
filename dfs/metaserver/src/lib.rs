pub mod dfs {
    include!(concat!(env!("OUT_DIR"), "/dfs.rs"));
}

pub mod config_server;
pub mod master;
pub mod sharding;
pub mod simple_raft;
