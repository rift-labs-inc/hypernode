use std::sync::Arc;
use std::time::Instant;

use bitcoin::{hashes::Hash, opcodes::all::OP_RETURN, script::Builder, Block};
use eyre::Result;
use log::info;

use crate::{btc_rpc::BitcoinRpcClient, core::ThreadSafeStore};

fn build_rift_inscription(order_nonce: [u8; 32]) -> Vec<u8> {
    Builder::new()
        .push_opcode(OP_RETURN)
        .push_slice(&order_nonce)
        .into_script().into_bytes()
}


async fn analyze_block_for_payments(
    block: &Block,
    active_reservations: Arc<ThreadSafeStore>,
) -> Result<()> {

    let expected_order_inscriptions = active_reservations
        .with_lock(|reservations_guard| {
            reservations_guard.reservations.iter().map(|(id, metadata)| {
                let script = build_rift_inscription(metadata.reservation.nonce.0);
                (id.clone(), script)
            }).collect::<Vec<_>>()
        })
        .await;

    for tx in block.txdata.iter() {
        for script in tx.output.iter().map(|out| out.script_pubkey.clone()) {
            for (id, expected_script) in expected_order_inscriptions.iter() {
                if script.as_bytes() == expected_script {
                    let block_height = block.bip34_block_height()? as u64;
                    info!("Found payment for reservation: {}, txid: {} at block height: {}", id, tx.compute_txid(), block_height);
                    active_reservations.with_lock(|reservations_guard| {
                        reservations_guard.update_btc_reservation(
                            *id,
                            block_height,
                            *block.block_hash().as_raw_hash().as_byte_array(),
                            *tx.compute_txid().as_byte_array(),
                        );
                    }).await;
                }
            }
        }
    }

    Ok(())
}

async fn analyze_reservations_for_sufficient_confirmations(
    block: &Block,
    active_reservations: Arc<ThreadSafeStore>,
) -> Result<()> {
    let reservations = active_reservations
        .with_lock(|reservations_guard| {
            reservations_guard.reservations.iter().map(|(id, metadata)| {
                (id.clone(), metadata.btc.clone())
            }).collect::<Vec<_>>()
        })
        .await;
    
    // 
    
    Ok(())
}

pub async fn find_block_height_from_time(rpc_url: &str, hours: u64) -> Result<u64> {
    let rpc = BitcoinRpcClient::new(rpc_url);
    let time = Instant::now();
    let current_block_height = rpc.get_block_count().await?;
    let current_block_hash = rpc.get_block_hash(current_block_height).await?;
    let current_block_timestamp = rpc.get_block(&current_block_hash).await?.header.time as u64;

    let target_timestamp = current_block_timestamp - hours * 3600;

    let blocks_per_hour = 60 / 10;
    let estimated_blocks_ago = hours * blocks_per_hour;

    let mut check_block = current_block_height - estimated_blocks_ago;

    loop {
        let block_hash = rpc.get_block_hash(check_block).await?;
        let block_timestamp = rpc.get_block(&block_hash).await?.header.time as u64;

        if block_timestamp <= target_timestamp {
            info!(
                "Found Bitcoin block height: {}, {} hours from tip in {:?}",
                check_block,
                (current_block_timestamp - block_timestamp) as f64 / 3600 as f64,
                time.elapsed()
            );
            return Ok(check_block);
        }

        check_block -= blocks_per_hour;
    }
}

// analyzes every btc block in the range [start_block_height, current_height] for reservation
// payments, once it's fully sync'd to the current tip, it will poll for new blocks every
// polling_interval seconds
pub async fn block_listener(
    rpc_url: &str,
    start_block_height: u64,
    polling_interval: u64,
    active_reservations: Arc<ThreadSafeStore>,
) -> Result<()> {
    // user should encode user & pass into rpc url if needed
    let rpc = BitcoinRpcClient::new(rpc_url);
    let mut current_height = rpc.get_block_count().await?;
    let mut analyzed_height = start_block_height - 1;
    let mut total_blocks_to_sync = current_height - analyzed_height;
    let mut fully_synced_logged = false;

    loop {
        if current_height > analyzed_height {
            analyzed_height += 1;
            let block = rpc
                .get_block(&rpc.get_block_hash(analyzed_height).await?)
                .await?;
            
            analyze_block_for_payments(&block, Arc::clone(&active_reservations)).await?;
            
            let blocks_synced = analyzed_height - start_block_height + 1;
            let progress_percentage = (blocks_synced as f64 / total_blocks_to_sync as f64) * 100.0;
            info!(
                "Syncing blocks: {:.2}% complete. Synced height: {}, Tip: {}",
                progress_percentage, analyzed_height, current_height
            );
            fully_synced_logged = false;
        } else {
            // we've caught up, check for new blocks
            let new_height = rpc.get_block_count().await?;
            if new_height > current_height {
                // new blocks available, update and continue syncing
                current_height = new_height;
                // Update total_blocks_to_sync for accurate percentage calculation
                total_blocks_to_sync = current_height - start_block_height + 1;
                info!("New blocks found. Continuing sync to new tip: {}", current_height);
                fully_synced_logged = false;
            } else if !fully_synced_logged {
                // we're fully synced, log only if not logged before
                info!("Fully synced. Waiting for new blocks...");
                fully_synced_logged = true;
            }
            // sleep and try again
            tokio::time::sleep(tokio::time::Duration::from_secs(polling_interval)).await;
        }
    }
}
