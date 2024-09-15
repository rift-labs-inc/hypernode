mod btc_indexer;
mod btc_rpc;
mod constants;
mod core;
mod evm_indexer;
mod proof_builder;
mod proof_broadcast;

use clap::Parser;
use constants::RESERVATION_DURATION_HOURS;
use core::{Store, ThreadSafeStore};
use dotenv;
use evm_indexer::fetch_token_decimals;
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

    /// Ethereum private key for signing transaction proofs
    #[arg(short, long, env)]
    private_key: String,

    /// Rift Exchange contract address
    #[arg(short, long, env)]
    rift_exchange_address: String,

    /// RPC concurrency limit
    #[arg(short, long, env, default_value = "10")]
    rpc_concurrency: usize,

    /// Bitcoin new block polling interval in seconds
    #[arg(short, long, env, default_value = "30")]
    btc_polling_interval: u64,

    /// Enable mock proof generation 
    #[arg(short, long, env, default_value= "false")]
    mock_proof: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::init();
    dotenv::dotenv().ok();
    let args = Args::parse();
    let rift_exchange_address = alloy::primitives::Address::from_str(&args.rift_exchange_address)?;

    let safe_store = Arc::new(ThreadSafeStore::new());

    let underyling_token_decimals =
        fetch_token_decimals(&args.evm_ws_rpc, &rift_exchange_address).await?;

    let proof_broadcast_queue = Arc::new(proof_broadcast::ProofBroadcastQueue::new(
        Arc::clone(&safe_store),
        args.evm_ws_rpc.clone(),
        rift_exchange_address,
        hex::decode(&args.private_key)?[..32].try_into()?,
    ));

    let proof_gen_queue = Arc::new(proof_builder::ProofGenerationQueue::new(
        Arc::clone(&safe_store),
        underyling_token_decimals,
        Arc::clone(&proof_broadcast_queue),
        args.mock_proof
    ));

    let (start_evm_block_height, start_btc_block_height) = tokio::try_join!(
        evm_indexer::find_block_height_from_time(&args.evm_ws_rpc, RESERVATION_DURATION_HOURS),
        btc_indexer::find_block_height_from_time(&args.btc_rpc, RESERVATION_DURATION_HOURS)
    )?;

    let synced_reservation_evm_height = evm_indexer::sync_reservations(
        &args.evm_ws_rpc,
        &rift_exchange_address,
        Arc::clone(&safe_store),
        start_evm_block_height,
        args.rpc_concurrency,
    )
    .await?;

    let synced_block_header_evm_height = evm_indexer::download_safe_bitcoin_headers(
        &args.evm_ws_rpc,
        &rift_exchange_address,
        Arc::clone(&safe_store),
        None,
        None,
    )
    .await?;

    tokio::try_join!(
        evm_indexer::exchange_event_listener(
            &args.evm_ws_rpc,
            rift_exchange_address,
            synced_reservation_evm_height,
            synced_block_header_evm_height,
            Arc::clone(&safe_store)
        ),
        btc_indexer::block_listener(
            &args.btc_rpc,
            start_btc_block_height,
            args.btc_polling_interval,
            Arc::clone(&safe_store),
            Arc::clone(&proof_gen_queue)
        )
    )?;
    Ok(())
}
