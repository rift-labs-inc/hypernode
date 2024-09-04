use std::sync::Arc;
use std::time::Instant;

use eyre::Result;
use log::info;
use serde_json::{self, Value};

use crate::{btc_rpc::BitcoinRpcClient, core::SafeActiveReservations};

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
    active_reservations: Arc<SafeActiveReservations>,
) -> Result<()> {
    // user should encode user & pass into rpc url if needed
    let rpc = BitcoinRpcClient::new(rpc_url);

    let mut current_height = rpc.get_block_count().await?;

    let mut analyzed_height = start_block_height - 1;

    loop {
        if current_height > analyzed_height {
            analyzed_height += 1;
            let block = rpc
                .get_block(&rpc.get_block_hash(analyzed_height).await?)
                .await?;
            // analyze the block...
        } else {
            // we've caught up, check for new blocks
            let new_height = rpc.get_block_count().await?;
            if new_height > current_height {
                // new blocks available, update and continue syncing
                current_height = new_height;
            } else {
                // we're fully synced, sleep and try again
                tokio::time::sleep(tokio::time::Duration::from_secs(polling_interval)).await;
            }
        }
    }
}
