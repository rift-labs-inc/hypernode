use std::time::SystemTime;

use eyre::Result;
use log::info;
use serde_json::{self, Value};

use crate::btc_rpc::{get_block, get_block_count, get_block_hash};

pub async fn find_block_height_from_time(rpc_url: &str, hours: u64) -> Result<u64> {
    let time = SystemTime::now();
    let current_block_height = get_block_count(rpc_url).await?;
    let current_block_hash = get_block_hash(rpc_url, current_block_height).await?;
    let current_block_timestamp = get_block(rpc_url, &current_block_hash).await?.header.time as u64;

    let target_timestamp = current_block_timestamp - hours * 3600;

    let blocks_per_hour = 60 / 10;
    let estimated_blocks_ago = hours * blocks_per_hour;

    let mut check_block = current_block_height - estimated_blocks_ago;

    loop {
        let block_hash = get_block_hash(rpc_url, check_block).await?;
        let block_timestamp = get_block(rpc_url, &block_hash).await?.header.time as u64;

        if block_timestamp <= target_timestamp {
            info!(
                "Found Bitcoin block height: {}, {} hours from tip in {} seconds",
                check_block,
                (current_block_timestamp-block_timestamp) as f64 / 3600 as f64,
                SystemTime::now().duration_since(time).unwrap().as_secs()
            );
            return Ok(check_block);
        }

        check_block -= blocks_per_hour;
    }
}

/*

pub async fn block_listener(rpc_url: &str, start_block_height: u64) -> Result<()>{
    // user should encode user & pass into rpc url if needed
    let client = Client::new(rpc_url.to_string(), Auth::None).await?;

    loop {
        let block = client.wait_for_new_block(0).await?;
        todo!() // sift through block and update global reservation state 
    }

}

*/
