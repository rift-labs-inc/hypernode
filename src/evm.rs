//! Example of multiplexing the watching of event logs.

use alloy::{
    primitives::{Address},
    providers::{ProviderBuilder, WsConnect},
    sol_types::SolEvent,
};
use eyre::Result;
use futures_util::StreamExt;

use crate::core::RiftExchange;

// Goes through the last n hours of worth of ethereum blocks to find
// any reservations
pub async fn sync_reservations() {
    todo!()
}


pub async fn exchange_event_indexer(ws_rpc_url: &str, contract_address: Address) -> Result<()> {
    let provider = ProviderBuilder::new().on_ws(WsConnect::new(ws_rpc_url)).await?;

    let contract = RiftExchange::new(contract_address, provider);

    println!("Rift deployed at: {}", contract.address());

    // Create filters for each event.
    let swap_complete_filter = contract.SwapComplete_filter().watch().await?;
    let liquidity_reserved_filter = contract.LiquidityReserved_filter().watch().await?;
    let proof_proposed_filter = contract.ProofProposed_filter().watch().await?;

    // Convert the filters into streams.
    let mut swap_complete_stream = swap_complete_filter.into_stream();
    let mut liquidity_reserved_stream = liquidity_reserved_filter.into_stream();
    let mut proof_proposed_stream = proof_proposed_filter.into_stream();

    let swap_complete_log = &RiftExchange::SwapComplete::SIGNATURE_HASH;
    let liquidity_reserved_log = &RiftExchange::LiquidityReserved::SIGNATURE_HASH;
    let proof_proposed_log = &RiftExchange::ProofProposed::SIGNATURE_HASH;

    // Use tokio::select! to multiplex the streams and capture the log
    // tokio::select! will return the first event that arrives from any of the streams
    // The for loop helps capture all the logs.
    loop {
        tokio::select! {
            Some(log) = swap_complete_stream.next() => {
                log?.1
            }
            Some(log) = liquidity_reserved_stream.next() => {
                log?.1
            }
            Some(log) = proof_proposed_stream.next() => {
                log?.1
            }
        };
    }
}

