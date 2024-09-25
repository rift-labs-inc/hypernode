use alloy::network::eip2718::Encodable2718;
use alloy::providers::WalletProvider;
use alloy::transports::{BoxTransport, BoxTransportConnect, Transport};
use alloy::{
    contract,
    eips::{BlockId, BlockNumberOrTag},
    network::Ethereum,
    network::TransactionBuilder,
    primitives::{Address, Bytes, U256},
    providers::{FilterPollerBuilder, Provider, ProviderBuilder, RootProvider, WsConnect},
    pubsub::PubSubFrontend,
    rpc::types::{BlockTransactionsKind, Filter, TransactionInput, TransactionRequest},
    sol_types::{SolEvent, SolValue},
};
use eyre::{Result, WrapErr};
use futures::stream::{self, TryStreamExt};
use futures_util::StreamExt;
use log::info;
use core::time;
use std::error::Error;
use std::time::{Instant, UNIX_EPOCH};
use std::{any::Any, collections::HashMap};
use std::{collections::HashSet, sync::Arc};
use tokio::time::Duration;
use tokio::{sync::Mutex, time::sleep};

use crate::core::{EvmHttpProvider, RiftExchangeWebsocket};
use crate::{constants::RESERVATION_DURATION_HOURS, core::RiftExchange::LiquidityReserved};
use crate::{
    constants::{CHALLENGE_PERIOD_MINUTES, HEADER_LOOKBACK_LIMIT},
    core::{
        BlockHashes, BlockHeaderAggregator, DepositVaultAggregator, ReservationMetadata,
        RiftExchange::{self, RiftExchangeInstance, SwapReservation},
        ThreadSafeStore,
    },
};

// non blocking
pub fn release_after_challenge_period(
    swap_reservation_index: U256,
    unlock_timestamp: u32,
    contract: Arc<RiftExchangeWebsocket>,
    flashbots_provider: Arc<Option<EvmHttpProvider>>,
) {
    tokio::spawn(async move {
        let buffer_seconds = 30 as u32; // buffer to ensure the ethereum block timestamp is
                                        // past the unlock timestamp
        let release_liquidity_timestamp = unlock_timestamp + buffer_seconds;
        let current_timestamp = UNIX_EPOCH.elapsed().unwrap().as_secs() as u32;
        let sleep_duration = if current_timestamp > release_liquidity_timestamp {
            0
        } else {
            release_liquidity_timestamp - current_timestamp
        };

        info!(
            "Releasing liquidity for reservation index: {} after challenge period, waiting for {:#?}",
            swap_reservation_index,
            time::Duration::from_secs(sleep_duration.into())
        );
        // get current utc timestamp
        sleep(Duration::from_secs(sleep_duration.into())).await;
        let txn_calldata = contract
            .releaseLiquidity(swap_reservation_index)
            .calldata()
            .to_owned();
        let tx_hash = match flashbots_provider.as_ref() {
            Some(flashbots_provider) => {
                info!("Releasing liquidity using Flashbots");
                let tx = TransactionRequest::default()
                    .to(*contract.address())
                    .input(TransactionInput::new(txn_calldata));
                let tx = contract.provider().fill(tx).await.unwrap();
                let tx_envelope = tx
                    .as_builder()
                    .unwrap()
                    .clone()
                    .build(&contract.provider().wallet())
                    .await
                    .unwrap();
                let tx_encoded = tx_envelope.encoded_2718();
                let pending = flashbots_provider
                    .send_raw_transaction(&tx_encoded)
                    .await
                    .unwrap()
                    .register()
                    .await
                    .unwrap();
                pending.tx_hash().clone()
            }
            None => {
                let tx = TransactionRequest::default()
                    .to(*contract.address())
                    .input(TransactionInput::new(txn_calldata));
                contract
                    .provider()
                    .send_transaction(tx)
                    .await
                    .unwrap()
                    .tx_hash()
                    .clone()
            }
        };
        info!("Liquidity released with evm tx hash: {}", tx_hash);
    });
}

pub async fn fetch_token_decimals(exchange: &RiftExchangeWebsocket) -> Result<u8> {
    Ok(exchange.TOKEN_DECIMALS().call().await?._0)
}

fn decode_vaults(encoded_vaults: Vec<u8>) -> Result<Vec<RiftExchange::DepositVault>> {
    let encoded_vaults = Vec::<Bytes>::abi_decode(&encoded_vaults, false)
        .map_err(|_| eyre::eyre!("Failed to decode vault byte array"))?;
    encoded_vaults
        .into_iter()
        .map(|bytes| {
            let vault = RiftExchange::DepositVault::abi_decode(&bytes.to_vec(), false)
                .map_err(|e| eyre::eyre!("Failed to decode deposit vault: {}", e))?;
            Ok(vault)
        })
        .collect()
}

pub async fn download_vaults(
    contract: Arc<RiftExchangeWebsocket>,
    vault_indices: Vec<u32>,
) -> Result<Vec<RiftExchange::DepositVault>> {
    let provider = contract.provider();
    let encoded_vaults = DepositVaultAggregator::deploy_builder(
        provider,
        vault_indices.clone(),
        *contract.address(),
    )
    .call()
    .await?;

    let vaults = decode_vaults(encoded_vaults.to_vec()).wrap_err("Failed to decode vaults")?;
    Ok(vaults)
}

fn decode_block_hashes(encoded_blocks: Vec<u8>) -> Result<Vec<[u8; 32]>> {
    <Vec<Bytes>>::abi_decode(&encoded_blocks, false)?
        .into_iter()
        .map(|bytes| {
            let mut hash = [0u8; 32];
            hash.copy_from_slice(&bytes);
            Ok(hash)
        })
        .collect()
}

// get available bitcoin headers stored on the rift exchange contract
pub async fn download_safe_bitcoin_headers(
    contract: Arc<RiftExchangeWebsocket>,
    store: Arc<ThreadSafeStore>,
    end_block_height: Option<u64>,
    lookback_count: Option<usize>,
) -> Result<u64> {
    let provider = contract.provider();
    let current_evm_tip = provider.get_block_number().await?;

    let stored_tip = match end_block_height {
        Some(height) => height,
        None => u64::from_be_bytes(
            contract
                .currentHeight()
                .call()
                .await?
                ._0
                .to_be_bytes::<32>()[32 - 8..]
                .try_into()?,
        ),
    };

    let lookback_limit = lookback_count.unwrap_or(HEADER_LOOKBACK_LIMIT);

    let mut heights = (0..lookback_limit)
        .map(|i| U256::from(stored_tip).saturating_sub(U256::from(i)))
        .collect::<Vec<_>>();

    // all the equivalent heights will be grouped, no need to sort
    heights.dedup();

    let encoded_blocks =
        BlockHeaderAggregator::deploy_builder(provider, heights.clone(), *contract.address())
            .call()
            .await?;

    let block_hashes =
        decode_block_hashes(encoded_blocks.to_vec()).wrap_err("Failed to decode block hashes")?;

    store
        .with_lock(|store_guard| {
            for (i, hash) in block_hashes.iter().enumerate() {
                if hash == &[0u8; 32] {
                    continue;
                }
                store_guard.safe_contract_block_hashes.insert(
                    u64::from_be_bytes(
                        heights[i].to_be_bytes::<32>()[32 - 8..].try_into().unwrap(),
                    ),
                    *hash,
                );
            }
        })
        .await;

    Ok(current_evm_tip)
}

// Downloads the actual reservation struct and all utilized deposit vaults
pub async fn download_reservation(
    reservation_id: U256,
    contract: Arc<RiftExchangeWebsocket>,
) -> Result<(U256, ReservationMetadata)> {
    let reservation = match contract.getReservation(reservation_id).call().await {
        Ok(res) => res._0,
        Err(e) => {
            return Err(eyre::eyre!(
                "Failed to get reservation with ID {:?}: {}",
                reservation_id,
                e
            )
            .into());
        }
    };

    let vault_indexes: Vec<u32> = reservation
        .vaultIndexes
        .iter()
        .map(|index| u32::from_be_bytes(index.to_be_bytes::<32>()[32 - 4..].try_into().unwrap()))
        .collect();

    match download_vaults(contract, vault_indexes).await {
        Ok(vaults) => Ok((
            reservation_id,
            ReservationMetadata::new(reservation, vaults),
        )),
        Err(e) => Err(e.into()),
    }
}

pub async fn find_block_height_from_time(
    rift_exchange: &RiftExchangeWebsocket,
    hours: u64,
) -> Result<u64> {
    let time = Instant::now();

    let provider = rift_exchange.provider();
    let current_block = provider
        .get_block(BlockId::latest(), BlockTransactionsKind::Hashes)
        .await?
        .unwrap();
    let current_timestamp = &current_block.header.timestamp;
    let target_timestamp = current_timestamp.saturating_sub(hours * 3600);

    // Estimate blocks per hour (assuming ~12 second block time)
    let blocks_per_hour = 3600 / 12;
    let estimated_blocks_ago = hours * blocks_per_hour;

    let mut check_block = current_block
        .header
        .number
        .unwrap()
        .saturating_sub(estimated_blocks_ago);

    loop {
        let block_info = provider
            .get_block(
                BlockId::Number(BlockNumberOrTag::Number(check_block)),
                BlockTransactionsKind::Hashes,
            )
            .await?
            .unwrap();
        let block_timestamp = block_info.header.timestamp;

        if block_timestamp <= target_timestamp || check_block == 0 {
            info!(
                "Found EVM block height: {}, {:.2} hours from tip in {:?}",
                check_block,
                (current_timestamp - block_timestamp) as f64 / 3600 as f64,
                time.elapsed()
            );
            return Ok(check_block.into());
        }

        // If we haven't gone back far enough, jump back by another hour's worth of blocks
        check_block = check_block.saturating_sub(blocks_per_hour);
    }
}

// Goes through the last RESERVATION_DURATION_HOURS worth of ethereum blocks and collects all reservations
pub async fn sync_reservations(
    contract: Arc<RiftExchangeWebsocket>,
    flashbots_provider: Arc<Option<EvmHttpProvider>>,
    safe_store: Arc<ThreadSafeStore>,
    start_block: u64,
    rpc_concurrency: usize,
) -> Result<u64> {
    let time = Instant::now();
    info!("Syncing reservations from block {}", start_block);
    let provider = contract.provider();
    let latest_block: u64 = provider.get_block_number().await?;
    // Create a filter to get logs from the last 8 hours for the specified contract
    let log_filter = Filter::new()
        .address(*contract.address())
        .from_block(start_block)
        .to_block(latest_block);

    let logs = provider.get_logs(&log_filter).await?;

    // Process logs to determine active reservation IDs
    let mut active_reservations_set: HashSet<U256> = HashSet::new();
    let mut reservations_in_challenge: HashSet<U256> = HashSet::new();

    for log in logs {
        match log.topic0() {
            Some(&RiftExchange::LiquidityReserved::SIGNATURE_HASH) => {
                let reservation_event: RiftExchange::LiquidityReserved =
                    log.log_decode()?.inner.data;
                active_reservations_set.insert(reservation_event.swapReservationIndex);
            }
            Some(&RiftExchange::ProofProposed::SIGNATURE_HASH) => {
                let proof_proposed_event: RiftExchange::ProofProposed =
                    log.log_decode()?.inner.data;
                // TODO: This is not mainnet safe, we need to validate that a proposed proof is not
                // malicious / part of an orphan chain, instead of just removing the reservation
                active_reservations_set.remove(&proof_proposed_event.swapReservationIndex);
                reservations_in_challenge.insert(proof_proposed_event.swapReservationIndex);
            }
            Some(&RiftExchange::SwapComplete::SIGNATURE_HASH) => {
                let swap_complete_event: RiftExchange::SwapComplete = log.log_decode()?.inner.data;
                active_reservations_set.remove(&swap_complete_event.swapReservationIndex);
                reservations_in_challenge.remove(&swap_complete_event.swapReservationIndex);
            }
            _ => {}
        }
    }

    // combine the active reservations and reservations in challenge into one set
    let reservations_to_download: HashSet<U256> = active_reservations_set
        .union(&reservations_in_challenge)
        .cloned()
        .collect();

    // these are fully active reservations that are not in the challenge period
    let active_reservations_ids: Vec<U256> = active_reservations_set.into_iter().collect();

    let reservations_to_download: Vec<U256> = reservations_to_download.into_iter().collect();

    // download all reservation data from the contract, both active and in challenge
    let downloaded_reservations: HashMap<U256, ReservationMetadata> =
        stream::iter(reservations_to_download)
            .map(|reservation_id| {
                let contract: Arc<RiftExchangeWebsocket> = Arc::clone(&contract);
                async move {
                    download_reservation(reservation_id, contract)
                        .await
                        .map_err(|e| {
                            info!("Failed to download reservation: {}", e);
                            e
                        })
                }
            })
            .buffer_unordered(rpc_concurrency)
            .try_collect()
            .await
            .map_err(|e: eyre::Report| e)?;

    let reservations = downloaded_reservations
        .clone()
        .into_iter()
        .filter(|(id, _)| active_reservations_ids.contains(id))
        .collect::<HashMap<U256, ReservationMetadata>>();

    let total_reservations = &reservations.len();

    // Update active_reservations
    safe_store
        .with_lock(|reservations_guard| {
            for (id, metadata) in reservations {
                reservations_guard.insert(id, metadata);
            }
            Ok(())
        })
        .await
        .map_err(|e: Box<dyn Error>| eyre::eyre!("Failed to update reservations: {}", e))?;

    info!(
        "Synced {} reservations in {:?}",
        total_reservations,
        time.elapsed()
    );

    // Now spawn a task to call releaseLiquidity on all reservations in a challenge period after
    // their challenge period ends
    downloaded_reservations
        .into_iter()
        .filter(|(id, _)| reservations_in_challenge.contains(id))
        .for_each(|(id, metadata)| {
            let unlock_timestamp = metadata.reservation.unlockTimestamp;
            let contract = Arc::clone(&contract);
            let flashbots_provider = Arc::clone(&flashbots_provider);
            release_after_challenge_period(id, unlock_timestamp, contract, flashbots_provider);
        });

    Ok(latest_block)
}

pub async fn exchange_event_listener(
    contract: Arc<RiftExchangeWebsocket>,
    flashbots_provider: Arc<Option<EvmHttpProvider>>,
    start_index_block_height: u64,
    start_block_header_height: u64,
    active_reservations: Arc<ThreadSafeStore>,
) -> Result<()> {
    info!(
        "Rift deployed at: {} on chain ID: {}",
        contract.address(),
        &contract.provider().get_chain_id().await?
    );

    // Create filters for each event.
    let swap_complete_filter = contract
        .SwapComplete_filter()
        .from_block(start_index_block_height)
        .watch()
        .await?;
    let liquidity_reserved_filter = contract
        .LiquidityReserved_filter()
        .from_block(start_index_block_height)
        .watch()
        .await?;
    let proof_proposed_filter = contract
        .ProofProposed_filter()
        .from_block(start_index_block_height)
        .watch()
        .await?;

    let blocks_added_filter = contract
        .BlocksAdded_filter()
        .from_block(start_block_header_height)
        .watch()
        .await?;

    // Convert the filters into streams.
    let mut swap_complete_stream = swap_complete_filter.into_stream();
    let mut liquidity_reserved_stream = liquidity_reserved_filter.into_stream();
    let mut proof_proposed_stream = proof_proposed_filter.into_stream();
    let mut blocks_added_stream = blocks_added_filter.into_stream();

    // Use a HashSet to keep track of already-processed logs
    let processed_logs = Arc::new(Mutex::new(HashSet::new()));

    loop {
        let contract = Arc::clone(&contract);
        let flashbots_provider = Arc::clone(&flashbots_provider);
        tokio::select! {
            Some(log) = swap_complete_stream.next() => {
                let log_data = log.clone()?;
                let log_identifier = (log_data.1.block_number, log_data.1.transaction_index, log_data.1.log_index);

                let mut processed_logs_guard = processed_logs.lock().await;
                if !processed_logs_guard.contains(&log_identifier) {
                    processed_logs_guard.insert(log_identifier);
                    drop(processed_logs_guard);

                    let swap_reservation_index = log_data.0.swapReservationIndex;
                    info!("SwapComplete with reservation index: {:?}", &swap_reservation_index);
                    active_reservations.with_lock(|reservations_guard| {
                        reservations_guard.remove(swap_reservation_index);
                    }).await;
                }
            }

            Some(log) = liquidity_reserved_stream.next() => {
                let log_data = log.clone()?;
                let log_identifier = (log_data.1.block_number, log_data.1.transaction_index, log_data.1.log_index);

                let mut processed_logs_guard = processed_logs.lock().await;
                if !processed_logs_guard.contains(&log_identifier) {
                    processed_logs_guard.insert(log_identifier);
                    drop(processed_logs_guard);

                    info!("LiquidityReserved w/ reservation index: {:?}", &log_data.0.swapReservationIndex);
                    let swap_reservation_index = log_data.0.swapReservationIndex;
                    let reservation = download_reservation(swap_reservation_index, Arc::clone(&contract)).await?;
                    active_reservations.with_lock(|reservations_guard| {
                        reservations_guard.insert(swap_reservation_index, reservation.1);
                    }).await;
                }
            }

            Some(log) = proof_proposed_stream.next() => {
                let log_data = log.clone()?;
                let log_identifier = (log_data.1.block_number, log_data.1.transaction_index, log_data.1.log_index);

                let mut processed_logs_guard = processed_logs.lock().await;
                if !processed_logs_guard.contains(&log_identifier) {
                    processed_logs_guard.insert(log_identifier);
                    drop(processed_logs_guard);

                    info!("ProofProposed w/ reservation index: {:?}", &log_data.0.swapReservationIndex);
                    let swap_reservation_index = log_data.0.swapReservationIndex;
                    let reservation_metadata = download_reservation(swap_reservation_index, Arc::clone(&contract)).await?;
                    active_reservations.with_lock(|reservations_guard| {
                        reservations_guard.insert(swap_reservation_index, reservation_metadata.1.clone());
                    }).await;

                    release_after_challenge_period(
                        swap_reservation_index,
                        reservation_metadata.1.reservation.unlockTimestamp,
                        Arc::clone(&contract),
                        Arc::clone(&flashbots_provider)
                    );
                }
            }

            Some(log) = blocks_added_stream.next() => {
                let log_data = log.clone()?;
                let log_identifier = (log_data.1.block_number, log_data.1.transaction_index, log_data.1.log_index);

                let mut processed_logs_guard = processed_logs.lock().await;
                if !processed_logs_guard.contains(&log_identifier) {
                    processed_logs_guard.insert(log_identifier);
                    drop(processed_logs_guard);

                    let blocks_added = log_data.0;
                    let count = blocks_added.count;
                    let end_block_height = u64::from_be_bytes((blocks_added.startBlockHeight + count).to_be_bytes::<32>()[32-8..].try_into().unwrap());
                    info!("BlocksAdded w/ confirmation height: {:?} and safe height: {:?}", end_block_height, blocks_added.startBlockHeight);
                    download_safe_bitcoin_headers(
                        Arc::clone(&contract),
                        Arc::clone(&active_reservations),
                        Some(end_block_height),
                        Some(u64::from_be_bytes(blocks_added.count.to_be_bytes::<32>()[32-8..].try_into().unwrap()) as usize)).await?;
                }
            }
        };
    }
}
