use clap::Parser;
use rust_hadoop::dfs::master_service_server::MasterServiceServer;
use rust_hadoop::master::MyMaster;
use tonic::transport::Server;

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    #[arg(short, long, default_value = "127.0.0.1:50051")]
    addr: String,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    let addr = args.addr.parse()?;
    let master = MyMaster::default();

    println!("Master listening on {}", addr);

    Server::builder()
        .add_service(MasterServiceServer::new(master))
        .serve(addr)
        .await?;

    Ok(())
}
