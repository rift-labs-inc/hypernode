mod btc_indexer;
mod btc_rpc;
mod constants;
mod core;
mod evm_indexer;

use clap::Parser;
use constants::RESERVATION_DURATION_HOURS;
use core::{ActiveReservations, SafeActiveReservations};
use dotenv;
use eyre::Result;
use log::{info, trace, warn};
use std::{str::FromStr, sync::Arc};
use tokio::sync::Mutex;

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Ethereum RPC Websocket URL for indexing and proposing proofs onchain
    #[arg(short, long, env)]
    evm_ws_rpc: String,

    /// Bitcoin RPC URL for indexing
    #[arg(short, long, env)]
    btc_rpc: String,
    /// Rift Exchange contract address
    #[arg(short, long, env)]
    rift_exchange_address: String,

    /// Build proofs locally, as opposed to using the SP1 prover network [WARNING: 128gb of ram and
    /// 32 cores required for local prover]
    #[arg(short, long, env, default_value = "false")]
    local_prover: bool,

    /// RPC concurrency limit
    #[arg(short, long, env, default_value = "10")]
    rpc_concurrency: usize,
}

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::init();
    dotenv::dotenv().ok();
    let args = Args::parse();
    let rift_exchange_address = alloy::primitives::Address::from_str(&args.rift_exchange_address)?;

    let (start_evm_block_height, start_btc_block_height) = tokio::try_join!(
        evm_indexer::find_block_height_from_time(&args.evm_ws_rpc, RESERVATION_DURATION_HOURS),
        btc_indexer::find_block_height_from_time(&args.btc_rpc, RESERVATION_DURATION_HOURS)
    )?;

    let safe_active_reservations = Arc::new(SafeActiveReservations::new());
    let synced_evm_height = evm_indexer::sync_reservations(
        &args.evm_ws_rpc,
        &rift_exchange_address,
        Arc::clone(&safe_active_reservations),
        start_evm_block_height,
        args.rpc_concurrency,
    )
    .await?;

    /*
    let evm_listener = evm::exchange_event_listener(
        &args.evm_ws_rpc,
        rift_exchange_address,
        synced_evm_height,
        Arc::clone(&safe_active_reservations),
        args.rpc_concurrency,
    );
    let bitcoin_listener = bitcoin::block_listener(&args.btc_rpc, start_evm_block_height);

    */
    Ok(())
}
