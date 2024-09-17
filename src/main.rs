mod btc_indexer;
mod btc_rpc;
mod constants;
mod core;
mod evm_indexer;
mod proof_broadcast;
mod proof_builder;

use alloy::{
    network::{Ethereum, EthereumWallet},
    providers::{Provider, ProviderBuilder, WsConnect},
    pubsub::PubSubFrontend,
    signers::{k256::elliptic_curve::ff::derive::bitvec::boxed, local::PrivateKeySigner},
};
use clap::Parser;
use constants::RESERVATION_DURATION_HOURS;
use core::{
    EvmHttpProvider, EvmWebsocketProvider, RiftExchange, RiftExchangeHttp, RiftExchangeWebsocket,
    Store, ThreadSafeStore,
};
use dotenv;
use evm_indexer::fetch_token_decimals;
use eyre::Result;
use log::{info, trace, warn};
use std::{str::FromStr, sync::Arc};
use tokio::sync::Mutex;

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Ethereum RPC websocket URL for indexing and proposing proofs onchain
    #[arg(short, long, env)]
    evm_ws_rpc: String,

    /// Bitcoin RPC URL for indexing
    #[arg(short, long, env)]
    btc_rpc: String,

    /// Ethereum private key for signing hypernode initiated transactions
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
    #[arg(short, long, env, default_value = "false")]
    mock_proof: bool,

    /// Utilize Flashbots to prevent frontrunning on propose + release transactions (recommended
    /// for mainnet)
    #[arg(short, long, env, default_value = "false")]
    flashbots: bool,

    /// Flashbots relay URL, required if flashbots is enabled, will only be utilized when
    /// broadcasting transactions
    #[arg(short, long, env)]
    flashbots_relay_rpc: Option<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::init();
    dotenv::dotenv().ok();
    let args = Args::parse();
    let rift_exchange_address = alloy::primitives::Address::from_str(&args.rift_exchange_address)?;

    let safe_store = Arc::new(ThreadSafeStore::new());

    // If flashbots is enabled, the flashbots relay URL is required
    let flashbots_url = if args.flashbots {
        Some(
            args.flashbots_relay_rpc
                .as_ref()
                .ok_or_else(|| {
                    eyre::eyre!("Flashbots relay URL is required when flashbots is enabled")
                })?
                .clone(),
        )
    } else {
        Option::None
    };

    let private_key: [u8; 32] =
        hex::decode(args.private_key.trim_start_matches("0x"))?[..32].try_into()?;

    let provider: Arc<EvmWebsocketProvider> = Arc::new(
        ProviderBuilder::new()
            .with_recommended_fillers()
            .wallet(EthereumWallet::from(
                PrivateKeySigner::from_bytes(&private_key.into()).unwrap(),
            ))
            .on_ws(WsConnect::new(&args.evm_ws_rpc))
            .await
            .expect("Failed to connect to WebSocket"),
    );

    let contract: Arc<RiftExchangeWebsocket> =
        Arc::new(RiftExchange::new(rift_exchange_address, provider.clone()));

    let flashbots_provider: Arc<Option<EvmHttpProvider>> = Arc::new(match flashbots_url {
        None => None,
        Some(url) => Some(
            ProviderBuilder::new()
                .with_recommended_fillers()
                .wallet(EthereumWallet::from(
                    PrivateKeySigner::from_bytes(&private_key.into()).unwrap(),
                ))
                .on_http(url.parse()?),
        ),
    });

    let proof_broadcast_queue = Arc::new(proof_broadcast::ProofBroadcastQueue::new(
        Arc::clone(&safe_store),
        Arc::clone(&flashbots_provider),
        Arc::clone(&contract),
    ));

    let proof_gen_queue = Arc::new(proof_builder::ProofGenerationQueue::new(
        Arc::clone(&safe_store),
        Arc::clone(&proof_broadcast_queue),
        args.mock_proof,
    ));

    let (start_evm_block_height, start_btc_block_height) = tokio::try_join!(
        evm_indexer::find_block_height_from_time(&contract, RESERVATION_DURATION_HOURS),
        btc_indexer::find_block_height_from_time(&args.btc_rpc, RESERVATION_DURATION_HOURS)
    )?;

    let synced_reservation_evm_height = evm_indexer::sync_reservations(
        Arc::clone(&contract),
        Arc::clone(&flashbots_provider),
        Arc::clone(&safe_store),
        start_evm_block_height,
        args.rpc_concurrency,
    )
    .await?;

    let synced_block_header_evm_height = evm_indexer::download_safe_bitcoin_headers(
        Arc::clone(&contract),
        Arc::clone(&safe_store),
        None,
        None,
    )
    .await?;

    tokio::try_join!(
        evm_indexer::exchange_event_listener(
            Arc::clone(&contract),
            Arc::clone(&flashbots_provider),
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
