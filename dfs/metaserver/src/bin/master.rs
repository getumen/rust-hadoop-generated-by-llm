use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use clap::Parser;
use dfs_metaserver::dfs::master_service_server::MasterServiceServer;
use dfs_metaserver::master::{MasterState, MyMaster};
use dfs_metaserver::simple_raft::{
    AppendEntriesArgs, ClusterInfo, Event, InstallSnapshotArgs, RaftNode, RequestVoteArgs,
    RpcMessage,
};
use prometheus::{Encoder, Gauge, Registry, TextEncoder};
use std::sync::{Arc, Mutex};
use tonic::transport::Server;

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    #[arg(short, long, default_value = "127.0.0.1:50051")]
    addr: String,

    #[arg(long, default_value = "1")]
    id: usize,

    #[arg(long, value_delimiter = ',')]
    peers: Vec<String>, // http://host:port

    #[arg(long, default_value = "8080")]
    http_port: u16,

    #[arg(long)]
    advertise_addr: Option<String>,

    #[arg(long, default_value = "/tmp/raft-logs")]
    storage_dir: String,

    #[arg(long, default_value = "shard-0")]
    shard_id: String,

    #[arg(long)]
    shard_config: Option<String>,
}

// Axum state for sharing the Raft channel
#[derive(Clone)]
struct AppState {
    raft_tx: tokio::sync::mpsc::Sender<Event>,
}

// Custom error type for Axum
struct InternalError;

impl IntoResponse for InternalError {
    fn into_response(self) -> Response {
        (StatusCode::INTERNAL_SERVER_ERROR, "Internal server error").into_response()
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    let addr = args.addr.parse()?;
    let advertise_addr = args.advertise_addr.unwrap_or_else(|| args.addr.clone());

    println!("Master node {} starting...", args.id);
    println!("Peers: {:?}", args.peers);
    println!("HTTP Port: {}", args.http_port);
    println!("Advertise Addr: {}", advertise_addr);
    println!("Storage Dir: {}", args.storage_dir);

    let state = {
        let mut master_state = MasterState::default();
        master_state.enter_safe_mode();
        Arc::new(Mutex::new(dfs_metaserver::simple_raft::AppState::Master(
            master_state,
        )))
    };
    let (raft_tx, raft_rx) = tokio::sync::mpsc::channel(100);

    let raft_tx_for_node = raft_tx.clone();
    let raft_tx_for_server = raft_tx.clone();
    let raft_tx_for_master = raft_tx.clone();

    // Filter out empty peer strings (e.g., when --peers "" is passed)
    let peers: Vec<String> = args
        .peers
        .iter()
        .filter(|p| !p.is_empty())
        .cloned()
        .collect();

    let mut raft_node = RaftNode::new(
        args.id,
        peers.clone(),
        advertise_addr,
        args.storage_dir.clone(),
        state.clone(),
        raft_rx,
        raft_tx_for_node,
    );

    // Start Raft Node
    tokio::spawn(async move {
        raft_node.run().await;
    });

    // Build Axum router for Raft RPC
    let app_state = AppState {
        raft_tx: raft_tx_for_server,
    };

    let app = Router::new()
        .route("/health", get(handle_health))
        .route("/metrics", get(handle_metrics))
        .route("/raft/state", get(handle_raft_state))
        .route("/raft/vote", post(handle_vote))
        .route("/raft/append", post(handle_append))
        .route("/raft/snapshot", post(handle_snapshot))
        .with_state(app_state);

    // Start HTTP Server for Raft RPC
    let http_addr: std::net::SocketAddr = ([0, 0, 0, 0], args.http_port).into();
    tokio::spawn(async move {
        let listener = tokio::net::TcpListener::bind(http_addr).await.unwrap();
        axum::serve(listener, app).await.unwrap();
    });

    // Load Shard Map
    let shard_map =
        dfs_metaserver::sharding::load_shard_map_from_config(args.shard_config.as_deref(), 100);
    let shard_map = Arc::new(Mutex::new(shard_map));

    let master = MyMaster::new(state, raft_tx_for_master, shard_map, args.shard_id.clone());

    println!("Master listening on {}", addr);

    Server::builder()
        .add_service(MasterServiceServer::new(master).max_decoding_message_size(100 * 1024 * 1024))
        .serve(addr)
        .await?;

    Ok(())
}

async fn handle_health() -> impl IntoResponse {
    (StatusCode::OK, "OK")
}

async fn handle_raft_state(
    State(app_state): State<AppState>,
) -> Result<Json<ClusterInfo>, InternalError> {
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    if app_state
        .raft_tx
        .send(Event::GetClusterInfo { reply_tx })
        .await
        .is_err()
    {
        return Err(InternalError);
    }

    match reply_rx.await {
        Ok(info) => Ok(Json(info)),
        _ => Err(InternalError),
    }
}

async fn handle_metrics(State(app_state): State<AppState>) -> Result<String, InternalError> {
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    if app_state
        .raft_tx
        .send(Event::GetClusterInfo { reply_tx })
        .await
        .is_err()
    {
        return Err(InternalError);
    }

    let info = match reply_rx.await {
        Ok(info) => info,
        _ => return Err(InternalError),
    };

    let registry = Registry::new();

    let role_gauge = Gauge::new(
        "raft_role",
        "Current Raft role (0=Follower, 1=Candidate, 2=Leader)",
    )
    .unwrap();
    let term_gauge = Gauge::new("raft_current_term", "Current Raft term").unwrap();
    let commit_index_gauge = Gauge::new("raft_commit_index", "Current commit index").unwrap();
    let last_applied_gauge = Gauge::new("raft_last_applied", "Last applied index").unwrap();
    let log_len_gauge = Gauge::new("raft_log_len", "Current log length").unwrap();
    let votes_gauge = Gauge::new(
        "raft_votes_received",
        "Number of votes received in current term",
    )
    .unwrap();

    registry.register(Box::new(role_gauge.clone())).unwrap();
    registry.register(Box::new(term_gauge.clone())).unwrap();
    registry
        .register(Box::new(commit_index_gauge.clone()))
        .unwrap();
    registry
        .register(Box::new(last_applied_gauge.clone()))
        .unwrap();
    registry.register(Box::new(log_len_gauge.clone())).unwrap();
    registry.register(Box::new(votes_gauge.clone())).unwrap();

    let role_val = match info.role {
        dfs_metaserver::simple_raft::Role::Follower => 0.0,
        dfs_metaserver::simple_raft::Role::Candidate => 1.0,
        dfs_metaserver::simple_raft::Role::Leader => 2.0,
    };
    role_gauge.set(role_val);
    term_gauge.set(info.current_term as f64);
    commit_index_gauge.set(info.commit_index as f64);
    last_applied_gauge.set(info.last_applied as f64);
    log_len_gauge.set(info.log_len as f64);
    votes_gauge.set(info.votes_received as f64);

    let mut buffer = vec![];
    let encoder = TextEncoder::new();
    encoder.encode(&registry.gather(), &mut buffer).unwrap();

    Ok(String::from_utf8(buffer).unwrap())
}

async fn handle_vote(
    State(app_state): State<AppState>,
    Json(args): Json<RequestVoteArgs>,
) -> Result<Json<serde_json::Value>, InternalError> {
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    if app_state
        .raft_tx
        .send(Event::Rpc {
            msg: RpcMessage::RequestVote(args),
            reply_tx: Some(reply_tx),
        })
        .await
        .is_err()
    {
        return Err(InternalError);
    }

    match reply_rx.await {
        Ok(RpcMessage::RequestVoteResponse(reply)) => {
            Ok(Json(serde_json::to_value(&reply).unwrap()))
        }
        _ => Err(InternalError),
    }
}

async fn handle_append(
    State(app_state): State<AppState>,
    Json(args): Json<AppendEntriesArgs>,
) -> Result<Json<serde_json::Value>, InternalError> {
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    if app_state
        .raft_tx
        .send(Event::Rpc {
            msg: RpcMessage::AppendEntries(args),
            reply_tx: Some(reply_tx),
        })
        .await
        .is_err()
    {
        return Err(InternalError);
    }

    match reply_rx.await {
        Ok(RpcMessage::AppendEntriesResponse(reply)) => {
            Ok(Json(serde_json::to_value(&reply).unwrap()))
        }
        _ => Err(InternalError),
    }
}

async fn handle_snapshot(
    State(app_state): State<AppState>,
    Json(args): Json<InstallSnapshotArgs>,
) -> Result<Json<serde_json::Value>, InternalError> {
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    if app_state
        .raft_tx
        .send(Event::Rpc {
            msg: RpcMessage::InstallSnapshot(args),
            reply_tx: Some(reply_tx),
        })
        .await
        .is_err()
    {
        return Err(InternalError);
    }

    match reply_rx.await {
        Ok(RpcMessage::InstallSnapshotResponse(reply)) => {
            Ok(Json(serde_json::to_value(&reply).unwrap()))
        }
        _ => Err(InternalError),
    }
}
