use crate::dfs::FileMetadata;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::io::Cursor;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct Node {
    pub addr: String,
}

openraft::declare_raft_types!(
    pub TypeConfig: D = Request, R = Response, NodeId = u64, Node = Node, Entry = openraft::Entry<TypeConfig>, SnapshotData = Cursor<Vec<u8>>
);

pub type Raft = openraft::Raft<TypeConfig>;

// Application Request (Log Entry)
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Request {
    CreateFile {
        path: String,
    },
    AllocateBlock {
        path: String,
        block_id: String,
        locations: Vec<String>,
    },
    RegisterChunkServer {
        address: String,
    },
}

// Application Response
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Response {
    Ok,
    FileAlreadyExists,
    FileNotFound,
    State(Option<FileMetadata>), // For read requests if needed, though usually read from state directly
}

impl fmt::Display for Request {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", self)
    }
}

impl fmt::Display for Response {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", self)
    }
}
