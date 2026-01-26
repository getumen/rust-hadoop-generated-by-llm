use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::post,
    Json, Router,
};
use clap::Parser;
use dfs_common::sharding::ShardMap;
use dfs_metaserver::config_server::MyConfigServer;
use dfs_metaserver::dfs::config_service_server::ConfigServiceServer;
use dfs_metaserver::simple_raft::{
    AppState, AppendEntriesArgs, Event, InstallSnapshotArgs, RaftNode, RequestVoteArgs, RpcMessage,
};
use std::sync::{Arc, Mutex};
use tonic::transport::Server;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    #[arg(short, long, default_value = "127.0.0.1:50052")] // Default port different from Master
    addr: String,

    #[arg(long, default_value = "1")]
    id: usize,

    #[arg(long, value_delimiter = ',')]
    peers: Vec<String>, // http://host:port

    #[arg(long, default_value = "8081")] // Default HTTP port different from Master
    http_port: u16,

    #[arg(long)]
    advertise_addr: Option<String>,

    #[arg(long, default_value = "/tmp/config-raft-logs")]
    storage_dir: String,
}

// Axum state for sharing the Raft channel
#[derive(Clone)]
struct AxumState {
    raft_tx: tokio::sync::mpsc::Sender<Event>,
    state: Arc<Mutex<AppState>>,
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
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "config_server=debug,dfs_metaserver=debug".into()),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();

    let args = Args::parse();
    let addr = args.addr.parse()?;
    let advertise_addr = args.advertise_addr.unwrap_or_else(|| args.addr.clone());

    let peers: Vec<String> = args.peers.into_iter().filter(|p| !p.is_empty()).collect();

    println!("Config Server node {} starting...", args.id);
    println!("Peers: {:?}", peers);
    println!("HTTP Port: {}", args.http_port);
    println!("Advertise Addr: {}", advertise_addr);
    println!("Storage Dir: {}", args.storage_dir);

    // Initialize with empty ShardMap (Range-based)
    let state = Arc::new(Mutex::new(AppState::Config(
        dfs_metaserver::simple_raft::ConfigStateInner {
            shard_map: ShardMap::new_range(),
            masters: std::collections::HashMap::new(),
        },
    )));
    let (raft_tx, raft_rx) = tokio::sync::mpsc::channel(100);

    let raft_tx_for_node = raft_tx.clone();
    let raft_tx_for_server = raft_tx.clone();
    let raft_tx_for_service = raft_tx.clone();

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
    let app_state = AxumState {
        raft_tx: raft_tx_for_server,
        state: state.clone(),
    };

    let app = Router::new()
        .route("/raft/vote", post(handle_vote))
        .route("/raft/append", post(handle_append))
        .route("/raft/snapshot", post(handle_snapshot))
        .route("/shards", axum::routing::get(handle_get_shards))
        .with_state(app_state);

    // Start HTTP Server for Raft RPC
    let http_addr: std::net::SocketAddr = ([0, 0, 0, 0], args.http_port).into();
    tokio::spawn(async move {
        let listener = tokio::net::TcpListener::bind(http_addr).await.unwrap();
        axum::serve(listener, app).await.unwrap();
    });

    let config_server = MyConfigServer::new(state, raft_tx_for_service);

    println!("Config Server listening on {}", addr);

    Server::builder()
        .add_service(ConfigServiceServer::new(config_server))
        .serve(addr)
        .await?;

    Ok(())
}

async fn handle_vote(
    State(app_state): State<AxumState>,
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
    State(app_state): State<AxumState>,
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
    State(app_state): State<AxumState>,
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

async fn handle_get_shards(
    State(app_state): State<AxumState>,
) -> Result<Json<serde_json::Value>, InternalError> {
    let state_lock = app_state.state.lock().unwrap();
    if let AppState::Config(ref config_state) = *state_lock {
        let shards = config_state.shard_map.get_all_shards();
        Ok(Json(serde_json::json!({ "shards": shards })))
    } else {
        Err(InternalError)
    }
}
