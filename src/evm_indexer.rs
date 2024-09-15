use alloy::{
    contract,
    eips::{BlockId, BlockNumberOrTag},
    network::Ethereum,
    primitives::{Address, Bytes, U256},
    providers::{FilterPollerBuilder, Provider, ProviderBuilder, RootProvider, WsConnect},
    pubsub::PubSubFrontend,
    rpc::types::{BlockTransactionsKind, Filter},
    sol_types::{SolEvent, SolValue},
};
use eyre::{Result, WrapErr};
use futures::stream::{self, TryStreamExt};
use futures_util::StreamExt;
use log::info;
use std::error::Error;
use std::time::{Instant, UNIX_EPOCH};
use std::{any::Any, collections::HashMap};
use std::{collections::HashSet, sync::Arc};
use tokio::sync::Mutex;
use tokio::time::Duration;

use crate::{
    constants::HEADER_LOOKBACK_LIMIT,
    core::{
        BlockHashes, BlockHeaderAggregator, DepositVaultAggregator, ReservationMetadata,
        RiftExchange::{self, RiftExchangeInstance, SwapReservation},
        ThreadSafeStore,
    },
};
use crate::{constants::RESERVATION_DURATION_HOURS, core::RiftExchange::LiquidityReserved};


pub async fn fetch_token_decimals(
    ws_rpc_url: &str,
    rift_exchange_address: &Address,
) -> Result<u8> {
    let provider = ProviderBuilder::new()
        .on_ws(WsConnect::new(ws_rpc_url))
        .await?;

    let rift_exchange = RiftExchange::new(*rift_exchange_address, &provider);
    Ok(rift_exchange.TOKEN_DECIMALS().call().await?._0)
}

fn decode_vaults(encoded_vaults: Vec<u8>) -> Result<Vec<RiftExchange::DepositVault>> {
    let encoded_vaults = Vec::<Bytes>::abi_decode(&encoded_vaults, false).map_err(|_| eyre::eyre!("Failed to decode vault byte array"))?;
    encoded_vaults.into_iter().map(|bytes| {
        let vault = RiftExchange::DepositVault::abi_decode(&bytes.to_vec(), false)
            .map_err(|e| eyre::eyre!("Failed to decode deposit vault: {}", e))?;
        Ok(vault)
    }).collect()
}

pub async fn download_vaults(
    ws_rpc_url: &str,
    rift_exchange_address: &Address,
    vault_indices: Vec<u32>,
) -> Result<Vec<RiftExchange::DepositVault>> {
    let provider = ProviderBuilder::new()
        .on_ws(WsConnect::new(ws_rpc_url))
        .await?;

    let encoded_vaults = DepositVaultAggregator::deploy_builder(
        provider,
        vault_indices.clone(),
        *rift_exchange_address,
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
    ws_rpc_url: &str,
    rift_exchange_address: &Address,
    store: Arc<ThreadSafeStore>,
    end_block_height: Option<u64>,
    lookback_count: Option<usize>,
) -> Result<u64> {
    let provider = ProviderBuilder::new()
        .on_ws(WsConnect::new(ws_rpc_url))
        .await?;

    let contract = RiftExchange::new(*rift_exchange_address, &provider);
    let current_evm_tip = provider.get_block_number().await?;

    let stored_tip = match end_block_height {
        Some(height) => height,
        None => u64::from_be_bytes(contract.currentHeight().call().await?._0.to_be_bytes::<32>()[32-8..].try_into().unwrap()),
    };


    let lookback_limit = lookback_count.unwrap_or(HEADER_LOOKBACK_LIMIT);

    let mut heights = (0..lookback_limit)
        .map(|i| U256::from(stored_tip).saturating_sub(U256::from(i)))
        .collect::<Vec<_>>();

    // all the equivalent heights will be grouped, no need to sort
    heights.dedup();

    let encoded_blocks =
        BlockHeaderAggregator::deploy_builder(provider, heights.clone(), *rift_exchange_address)
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
                store_guard
                    .safe_contract_block_hashes
                    .insert(u64::from_be_bytes(heights[i].to_be_bytes::<32>()[32-8..].try_into().unwrap()), *hash);
            }
        })
        .await;

    Ok(current_evm_tip)
}

// Downloads the actual reservation struct and all utilized deposit vaults
pub async fn download_reservation(
    reservation_id: U256,
    ws_rpc_url: &str,
    contract_address: &Address,
) -> Result<(U256, ReservationMetadata)> {
    let provider = ProviderBuilder::new()
        .on_ws(WsConnect::new(ws_rpc_url))
        .await?;

    let contract = RiftExchange::new(*contract_address, &provider);

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
        .map(|index| {
            u32::from_be_bytes(index.to_be_bytes::<32>()[32-4..].try_into().unwrap())
        })
        .collect();

    match download_vaults(ws_rpc_url, contract_address, vault_indexes).await {
        Ok(vaults) => {
            // TODO: download liquidity providers and store them, also abstract out this logic to a
            // function that can be used here and in the listener
            Ok((
                reservation_id,
                ReservationMetadata::new(reservation, vaults),
            ))
        }
        Err(e) => Err(e.into()),
    }
}

pub async fn find_block_height_from_time(ws_rpc_url: &str, hours: u64) -> Result<u64> {
    let time = Instant::now();
    let provider = Arc::new(
        ProviderBuilder::new()
            .on_ws(WsConnect::new(ws_rpc_url))
            .await?,
    );

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

        if block_timestamp <= target_timestamp {
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
    ws_rpc_url: &str,
    contract_address: &Address,
    active_reservations: Arc<ThreadSafeStore>,
    start_block: u64,
    rpc_concurrency: usize,
) -> Result<u64> {
    let time = Instant::now();
    info!("Syncing reservations from block {}", start_block);
    let provider = Arc::new(
        ProviderBuilder::new()
            .on_ws(WsConnect::new(ws_rpc_url))
            .await?,
    );
    let contract = Arc::new(RiftExchange::new(*contract_address, provider.clone()));
    let latest_block: u64 = provider.get_block_number().await?;
    // Create a filter to get logs from the last 8 hours for the specified contract
    let log_filter = Filter::new()
        .address(*contract_address)
        .from_block(start_block)
        .to_block(latest_block);

    // Fetch the logs
    let logs = provider.get_logs(&log_filter).await?;

    // Process logs to determine active reservation IDs
    let mut active_reservations_set: HashSet<U256> = HashSet::new();

    for log in logs {
        match log.topic0() {
            Some(&RiftExchange::LiquidityReserved::SIGNATURE_HASH) => {
                let reservation_event: RiftExchange::LiquidityReserved =
                    log.log_decode()?.inner.data;
                active_reservations_set.insert(reservation_event.swapReservationIndex);
            }
            Some(&RiftExchange::SwapComplete::SIGNATURE_HASH) => {
                let swap_complete_event: RiftExchange::SwapComplete = log.log_decode()?.inner.data;
                active_reservations_set.remove(&swap_complete_event.swapReservationIndex);
            }
            _ => {}
        }
    }

    // the rest of these are fully active or currently in a challenge period
    let active_reservations_ids: Vec<U256> = active_reservations_set.into_iter().collect();

    // download all reservation data from the contract
    let reservations: HashMap<U256, ReservationMetadata> = stream::iter(active_reservations_ids)
        .map(|reservation_id| {
            let contract = Arc::clone(&contract);
            async move {
                download_reservation(reservation_id, ws_rpc_url, contract.address())
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

    let total_reservations = &reservations.len();

    // Update active_reservations
    active_reservations
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

    Ok(latest_block)
}

pub async fn exchange_event_listener(
    ws_rpc_url: &str,
    contract_address: Address,
    start_index_block_height: u64,
    start_block_header_height: u64,
    active_reservations: Arc<ThreadSafeStore>,
) -> Result<()> {
    let provider = ProviderBuilder::new()
        .on_ws(WsConnect::new(ws_rpc_url))
        .await?;

    let contract = RiftExchange::new(contract_address, &provider);

    info!(
        "Rift deployed at: {} on chain ID: {}",
        contract.address(),
        &provider.get_chain_id().await?
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
                    let reservation = download_reservation(swap_reservation_index, ws_rpc_url, &contract_address).await?; 
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
                    let reservation = download_reservation(swap_reservation_index, ws_rpc_url, &contract_address).await?; 
                    active_reservations.with_lock(|reservations_guard| {
                        reservations_guard.insert(swap_reservation_index, reservation.1);
                    }).await;
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
                        ws_rpc_url,
                        &contract_address,
                        Arc::clone(&active_reservations),
                        Some(end_block_height),
                        Some(u64::from_be_bytes(blocks_added.count.to_be_bytes::<32>()[32-8..].try_into().unwrap()) as usize)).await?;
                }
            }
        };
    }
}
