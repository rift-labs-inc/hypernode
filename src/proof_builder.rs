use std::sync::Arc;

use alloy::{primitives::U256};
use bitcoin::{hashes::Hash, Block};
use eyre::Result;

use crate::{btc_rpc::BitcoinRpcClient, core::SafeActiveReservations};

async fn analyze_block(
    rpc: &BitcoinRpcClient,
    block: &Block,
    active_reservations: Arc<SafeActiveReservations>,
) -> Result<()> {
    let block_height = block.bip34_block_height()?;
    
    let mut block_hash = block.header.block_hash().to_raw_hash().to_byte_array();
    let mut block_timestamp = block.header.time as u64;

    for tx in block.txdata.iter() {
        for vout in tx.output.iter() {
            if let Some(script_pub_key) = vout.script_pubkey {
                if let Some(address) = script_pub_key.addresses {
                    for addr in address {
                        if let Some(reservation_id) = U256::from_str(&addr) {
                            let reservation = active_reservations
                                .with_lock(|reservations_guard| {
                                    reservations_guard.get(&reservation_id).cloned()
                                })
                                .await;

                            if let Some(reservation) = reservation {
                                let mut metadata = reservation;
                                metadata.proposed_block_height = Some(block_height);
                                active_reservations
                                    .with_lock(|reservations_guard| {
                                        reservations_guard.insert(reservation_id, metadata);
                                        Ok(())
                                    })
                                    .await?;
                            }
                        }
                    }
                }
            }
        }
    }

    Ok(())
}

