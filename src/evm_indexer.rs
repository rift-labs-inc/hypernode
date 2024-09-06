use alloy::{
    contract,
    eips::{BlockId, BlockNumberOrTag},
    network::Ethereum,
    primitives::{Address, U256},
    providers::{FilterPollerBuilder, Provider, ProviderBuilder, RootProvider, WsConnect},
    pubsub::PubSubFrontend,
    rpc::types::{BlockTransactionsKind, Filter},
    sol_types::SolEvent,
};
use eyre::{Result, WrapErr};
use futures::stream::{self, TryStreamExt};
use futures_util::StreamExt;
use log::info;
use std::error::Error;
use std::time::{Instant, UNIX_EPOCH};
use std::{any::Any, collections::HashMap};
use std::{collections::HashSet, sync::Arc};
use tokio::time::Duration;

use crate::core::{
    ReservationMetadata,
    RiftExchange::{self, RiftExchangeInstance, SwapReservation},
    SafeActiveReservations,
};
use crate::{constants::RESERVATION_DURATION_HOURS, core::RiftExchange::LiquidityReserved};

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
    active_reservations: Arc<SafeActiveReservations>,
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
                let reservation = contract.getReservation(reservation_id).call().await?._0;

                Ok((
                    reservation_id, 
                    ReservationMetadata::new(reservation),
                )) as Result<(U256, ReservationMetadata), contract::Error>
            }
        })
        .buffer_unordered(rpc_concurrency)
        .try_collect()
        .await?;

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
    active_reservations: Arc<SafeActiveReservations>,
    rpc_concurrency: usize,
) -> Result<()> {
    let provider = ProviderBuilder::new()
        .on_ws(WsConnect::new(ws_rpc_url))
        .await?;

    let contract = RiftExchange::new(contract_address, &provider);

    info!("Rift deployed at: {} on chain ID: {}", contract.address(), &provider.get_chain_id().await?);

    // Create filters for each event.
    let swap_complete_filter = contract.SwapComplete_filter().from_block(start_index_block_height).watch().await?;
    let liquidity_reserved_filter = contract.LiquidityReserved_filter().from_block(start_index_block_height).watch().await?;
    let proof_proposed_filter = contract.ProofProposed_filter().from_block(start_index_block_height).watch().await?;

    // Convert the filters into streams.
    let mut swap_complete_stream = swap_complete_filter.into_stream();
    let mut liquidity_reserved_stream = liquidity_reserved_filter.into_stream();
    let mut proof_proposed_stream = proof_proposed_filter.into_stream();

    // Use tokio::select! to multiplex the streams and capture the log
    // tokio::select! will return the first event that arrives from any of the streams
    // The for loop helps capture all the logs.
    loop {
        tokio::select! {
            Some(log) = swap_complete_stream.next() => {
                let swap_reservation_index = log.clone()?.0.swapReservationIndex;
                info!("SwapComplete with reservation index: {:?}", &swap_reservation_index);
                active_reservations.with_lock(|reservations_guard| {
                    reservations_guard.remove(swap_reservation_index);
                }).await;
            }
            Some(log) = liquidity_reserved_stream.next() => {
                info!("LiquidityReserved w/ reservation index: {:?}", &log.clone()?.0.swapReservationIndex);
                let swap_reservation_index = log.clone()?.0.swapReservationIndex;
                let reservation = contract.getReservation(swap_reservation_index).call().await?._0;
                active_reservations.with_lock(|reservations_guard| {
                    reservations_guard.insert(swap_reservation_index, ReservationMetadata::new(reservation));
                }).await;
            }
            Some(log) = proof_proposed_stream.next() => {
                info!("ProofProposed w/ reservation index: {:?}", &log.clone()?.0.swapReservationIndex);
                let swap_reservation_index = log.clone()?.0.swapReservationIndex;
                let reservation = contract.getReservation(swap_reservation_index).call().await?._0;
                active_reservations.with_lock(|reservations_guard| {
                    reservations_guard.insert(swap_reservation_index, ReservationMetadata::new(reservation));
                }).await;
            }
        };
    }
}
