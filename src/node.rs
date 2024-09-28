use alloy::{
    network::EthereumWallet,
    providers::{ProviderBuilder, WsConnect},
    signers::local::PrivateKeySigner,
};
use eyre::Result;
use std::{str::FromStr, sync::Arc};

use crate::constants::RESERVATION_DURATION_HOURS;
use crate::HypernodeArgs;
use crate::core::{
    EvmHttpProvider, EvmWebsocketProvider, RiftExchange, RiftExchangeWebsocket,
    ThreadSafeStore,
};
use crate::{evm_indexer, btc_indexer, proof_broadcast, proof_builder};

pub async fn run(args: HypernodeArgs) -> Result<()> {
    let rift_exchange_address = alloy::primitives::Address::from_str(&args.rift_exchange_address)?;

    let safe_store = Arc::new(ThreadSafeStore::new());

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
        None
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
        args.proof_gen_concurrency
    ));

    let (start_evm_block_height, start_btc_block_height) = tokio::try_join!(
        evm_indexer::find_block_height_from_time(&contract, RESERVATION_DURATION_HOURS, args.evm_block_time),
        btc_indexer::find_block_height_from_time(&args.btc_rpc, RESERVATION_DURATION_HOURS, args.btc_block_time)
    )?;

    let synced_reservation_evm_height = evm_indexer::sync_reservations(
        Arc::clone(&contract),
        Arc::clone(&flashbots_provider),
        Arc::clone(&safe_store),
        start_evm_block_height,
        args.evm_rpc_concurrency,
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
            Arc::clone(&proof_gen_queue),
            args.btc_rpc_concurrency
        )
    )?;

    Ok(())
}
