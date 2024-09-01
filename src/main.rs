mod evm;
mod bitcoin;
mod core;
mod constants;


use eyre::Result;
use clap::Parser;
use dotenv;

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Ethereum RPC Websocket URL for indexing and proposing proofs
    #[arg(short, long, env)]
    evm_ws_rpc: String,

    /// Bitcoin RPC URL for indexing
    #[arg(short, long, env)]
    btc_rpc: String,

    /// Rift Exchange contract address 
    #[arg(short, long, env)]
    rift_exchange_address: String,

    /// Build proofs locally, as opposed to default SP1 prover network
    #[arg(short, long, env, default_value = "false")]
    local_prover: bool
}

#[tokio::main]
async fn main() -> Result<()> {
    dotenv::dotenv().ok();
    let args = Args::parse();



    Ok(())
}
