pub mod dfs {
    include!(concat!(env!("OUT_DIR"), "/dfs.rs"));
}

pub mod config_server;
pub mod master;
pub mod simple_raft;

// ============================================================================
// Common Type Aliases
// ============================================================================

use std::sync::{Arc, Mutex};

/// Shared application state handle.
/// Used for thread-safe access to the Raft state machine's application state.
pub type SharedAppState = Arc<Mutex<simple_raft::AppState>>;

/// Shared shard map handle.
/// Used for thread-safe access to the cluster's shard configuration.
pub type SharedShardMap = Arc<Mutex<dfs_common::sharding::ShardMap>>;

/// Result type for Raft operations.
pub type RaftResult<T> = anyhow::Result<T>;
