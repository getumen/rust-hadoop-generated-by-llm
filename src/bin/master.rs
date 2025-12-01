use clap::Parser;
use rust_hadoop::dfs::master_service_server::MasterServiceServer;
use rust_hadoop::master::{MyMaster, MasterState};
use rust_hadoop::simple_raft::{RaftNode, Event, RpcMessage, RequestVoteArgs, AppendEntriesArgs};
use std::sync::{Arc, Mutex};
use tonic::transport::Server;
use warp::Filter;

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
}

#[derive(Debug)]
struct InternalError;
impl warp::reject::Reject for InternalError {}

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

    let state = Arc::new(Mutex::new(MasterState::default()));
    let (raft_tx, raft_rx) = tokio::sync::mpsc::channel(100);
    
    let raft_tx_for_node = raft_tx.clone();
    let raft_tx_for_server = raft_tx.clone();
    let raft_tx_for_master = raft_tx.clone();

    let mut raft_node = RaftNode::new(args.id, args.peers.clone(), advertise_addr, args.storage_dir.clone(), state.clone(), raft_rx, raft_tx_for_node);
    
    // Start Raft Node
    tokio::spawn(async move {
        raft_node.run().await;
    });

    // Start HTTP Server for Raft RPC
    let raft_tx_filter = warp::any().map(move || raft_tx_for_server.clone());

    let vote_route = warp::post()
        .and(warp::path("raft"))
        .and(warp::path("vote"))
        .and(warp::body::json())
        .and(raft_tx_filter.clone())
        .and_then(handle_vote);

    let append_route = warp::post()
        .and(warp::path("raft"))
        .and(warp::path("append"))
        .and(warp::body::json())
        .and(raft_tx_filter.clone())
        .and_then(handle_append);

    let routes = vote_route.or(append_route).boxed();
    
    tokio::spawn(async move {
        warp::serve(routes).run(([0, 0, 0, 0], args.http_port)).await;
    });

    let master = MyMaster::new(state, raft_tx_for_master);

    println!("Master listening on {}", addr);

    Server::builder()
        .add_service(
            MasterServiceServer::new(master)
                .max_decoding_message_size(100 * 1024 * 1024)
        )
        .serve(addr)
        .await?;

    Ok(())
}

async fn handle_vote(args: RequestVoteArgs, tx: tokio::sync::mpsc::Sender<Event>) -> Result<impl warp::Reply, warp::Rejection> {
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    if let Err(_) = tx.send(Event::Rpc {
        msg: RpcMessage::RequestVote(args),
        reply_tx: Some(reply_tx),
    }).await {
        return Err(warp::reject::custom(InternalError));
    }
    
    match reply_rx.await {
        Ok(RpcMessage::RequestVoteResponse(reply)) => Ok(warp::reply::json(&reply)),
        _ => Err(warp::reject::custom(InternalError)),
    }
}

async fn handle_append(args: AppendEntriesArgs, tx: tokio::sync::mpsc::Sender<Event>) -> Result<impl warp::Reply, warp::Rejection> {
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    if let Err(_) = tx.send(Event::Rpc {
        msg: RpcMessage::AppendEntries(args),
        reply_tx: Some(reply_tx),
    }).await {
        return Err(warp::reject::custom(InternalError));
    }
    
    match reply_rx.await {
        Ok(RpcMessage::AppendEntriesResponse(reply)) => Ok(warp::reply::json(&reply)),
        _ => Err(warp::reject::custom(InternalError)),
    }
}
