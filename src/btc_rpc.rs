use eyre::Result;
use rand::Rng;
use serde_json::Value;
use bitcoin::Block;
use bitcoin::consensus::deserialize;

pub async fn get_block_count(rpc_url: &str) -> Result<u64> {
    let mut rng = rand::thread_rng();
    let client = reqwest::Client::new();
    let response = client
        .post(rpc_url)
        .json(&serde_json::json!({
            "jsonrpc": "1.0",
            "id": rng.gen::<u64>(),
            "method": "getblockcount",
            "params": []
        }));

    let resp: Value = response.send().await?.json().await?;
    Ok(resp["result"].as_u64().unwrap())
}

pub async fn get_block_hash(rpc_url: &str, block_height: u64) -> Result<[u8; 32]> {
    let mut rng = rand::thread_rng();
    let client = reqwest::Client::new();
    let response = client
        .post(rpc_url)
        .json(&serde_json::json!({
            "jsonrpc": "1.0",
            "id": rng.gen::<u64>(),
            "method": "getblockhash",
            "params": [block_height]
        }));
    let resp: Value = response.send().await?.json().await?;
    let block_hexstr = resp["result"].as_str();
    if block_hexstr.is_none() {
        return Err(eyre::eyre!("Block hash doesn't exist"));
    }
    let block_hash: [u8; 32] = hex::decode(block_hexstr.unwrap())?.try_into().map_err(|_| eyre::eyre!("Invalid block hash"))?;
    Ok(block_hash)
}

pub async fn get_block(rpc_url: &str, block_hash: &[u8; 32]) -> Result<Block> {
    let mut rng = rand::thread_rng();
    let client = reqwest::Client::new();
    let response = client
        .post(rpc_url)
        .json(&serde_json::json!({
            "jsonrpc": "1.0",
            "id": rng.gen::<u64>(),
            "method": "getblock",
            "params": [hex::encode(block_hash), 0]
        }));
    let resp: Value = response.send().await?.json().await?;
    let block_hexstr = resp["result"].as_str();
    if block_hexstr.is_none() {
        return Err(eyre::eyre!("Block hash doesn't exist"));
    }
    let block_bytes = hex::decode(block_hexstr.unwrap())?;
    deserialize::<Block>(&block_bytes).map_err(|_| eyre::eyre!("Failed to deserialize block"))
}

#[cfg(test)]
mod tests {
    use bitcoin::hashes::Hash;

    use super::*;

    #[tokio::test]
    async fn test_get_block_count() {
        let rpc_url = "https://bitcoin-mainnet.public.blastapi.io";
        let block_count = get_block_count(rpc_url).await.unwrap();
        assert!(block_count > 0);
    }

    #[tokio::test]
    async fn test_get_block_hash() {
        let rpc_url = "https://bitcoin-mainnet.public.blastapi.io";
        let block_height = 859812;
        let block_hash = get_block_hash(rpc_url, block_height).await.unwrap();
        assert_eq!(block_hash.len(), 32);
    }

    #[tokio::test]
    async fn test_get_block() {
        let rpc_url = "https://bitcoin-mainnet.public.blastapi.io";
        let block_height = 859812;
        let mut block_hash = get_block_hash(rpc_url, block_height).await.unwrap();
        let block = get_block(rpc_url, &block_hash).await.unwrap();
        // reverse so it matches the native byte order
        block_hash.reverse(); 
        assert_eq!(*block.header.block_hash().as_raw_hash().as_byte_array(), block_hash);
    }
}
