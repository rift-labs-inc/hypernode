use std::any::Any;

use anyhow::Context;
use bitcoin::consensus::deserialize;
use bitcoin::Block;
use eyre::{eyre, Result, WrapErr};
use rand::rngs::ThreadRng;
use rand::Rng;
use reqwest::Client;
use serde_json::Value;

pub struct BitcoinRpcClient {
    client: Client,
    rng: ThreadRng,
    rpc_url: String,
}

impl BitcoinRpcClient {
    pub fn new(rpc_url: &str) -> Self {
        Self {
            client: Client::new(),
            rng: rand::thread_rng(),
            rpc_url: rpc_url.to_string(),
        }
    }

    async fn send_request(&self, method: &str, params: Value) -> Result<Value> {
        let response = self
            .client
            .post(&self.rpc_url)
            .json(&serde_json::json!({
                "jsonrpc": "1.0",
                "id": self.rng.clone().gen::<u64>(),
                "method": method,
                "params": params
            }))
            .send()
            .await
            .wrap_err("Failed to send request")?;

        let status = response.status();
        let text = response
            .text()
            .await
            .wrap_err("Failed to get response body")?;

        let json: Value = serde_json::from_str(&text).wrap_err_with(|| {
            eyre!(
                "Failed to parse JSON. Method: {}, Params: {:?}, Status: {}, Body: {}",
                method,
                params,
                status,
                text
            )
        })?;

        if let Some(error) = json.get("error") {
            if !error.is_null() {
                return Err(eyre!(
                    "RPC error. Method: {}, Params: {:?}, Status: {}, Body: {}",
                    method,
                    params,
                    status,
                    text
                ));
            }
        }

        json.get("result").cloned().ok_or_else(|| {
            eyre!(
                "No 'result' in response. Method: {}, Params: {:?}, Status: {}, Body: {}",
                method,
                params,
                status,
                text
            )
        })
    }
    pub async fn get_block_count(&self) -> Result<u64> {
        let result = self
            .send_request("getblockcount", Value::Array(vec![]))
            .await?;
        result
            .as_u64()
            .ok_or_else(|| eyre::eyre!("Invalid block count"))
    }

    pub async fn get_block_hash(&self, block_height: u64) -> Result<[u8; 32]> {
        let result = self
            .send_request("getblockhash", Value::Array(vec![block_height.into()]))
            .await?;
        let block_hexstr = result
            .as_str()
            .ok_or_else(|| eyre::eyre!("Block hash doesn't exist"))?;
        let block_hash: [u8; 32] = hex::decode(block_hexstr)?
            .try_into()
            .map_err(|_| eyre::eyre!("Invalid block hash"))?;
        Ok(block_hash)
    }

    pub async fn get_block(&self, block_hash: &[u8; 32]) -> Result<Block> {
        let result = self
            .send_request(
                "getblock",
                Value::Array(vec![hex::encode(block_hash).into(), 0.into()]),
            )
            .await?;
        let block_hexstr = result
            .as_str()
            .ok_or_else(|| eyre::eyre!("Block doesn't exist"))?;
        let block_bytes = hex::decode(block_hexstr)?;
        deserialize::<Block>(&block_bytes).map_err(|_| eyre::eyre!("Failed to deserialize block"))
    }

    pub async fn get_chainwork(&self, block_hash: &[u8; 32]) -> Result<[u8; 32]> {
        let result = self
            .send_request(
                "getblockheader",
                Value::Array(vec![hex::encode(block_hash).into()]),
            )
            .await?;
        let chainwork_hexstr = result
            .get("chainwork")
            .and_then(|v| v.as_str())
            .ok_or_else(|| eyre::eyre!("Chainwork doesn't exist"))?;
        let chainwork: [u8; 32] = hex::decode(chainwork_hexstr)?.as_slice().try_into()?;
        Ok(chainwork)
    }
    
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitcoin::hashes::Hash;

    #[tokio::test]
    async fn test_get_block_count() {
        let client = BitcoinRpcClient::new("https://bitcoin-mainnet.public.blastapi.io");
        let block_count = client.get_block_count().await.unwrap();
        assert!(block_count > 0);
    }

    #[tokio::test]
    async fn test_get_block_hash() {
        let client = BitcoinRpcClient::new("https://bitcoin-mainnet.public.blastapi.io");
        let block_height = 859812;
        let block_hash = client.get_block_hash(block_height).await.unwrap();
        assert_eq!(block_hash.len(), 32);
    }

    #[tokio::test]
    async fn test_get_block() {
        let client = BitcoinRpcClient::new("https://bitcoin-mainnet.public.blastapi.io");
        let block_height = 859812;
        let mut block_hash = client.get_block_hash(block_height).await.unwrap();
        let block = client.get_block(&block_hash).await.unwrap();
        // reverse so it matches the native byte order
        block_hash.reverse();
        assert_eq!(
            *block.header.block_hash().as_raw_hash().as_byte_array(),
            block_hash
        );
    }

    #[tokio::test]
    async fn test_get_chainwork() {
        let client = BitcoinRpcClient::new("https://bitcoin-mainnet.public.blastapi.io");
        let block_height = 859812;
        let block_hash = client.get_block_hash(block_height).await.unwrap();
        let chainwork = client.get_chainwork(&block_hash).await.unwrap();
        assert_eq!(chainwork.len(), 32);
    }
}
