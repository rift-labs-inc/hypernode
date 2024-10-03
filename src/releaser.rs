// Calls releaseLiquidity once enough evm blocks have passed
use crate::core::{EvmHttpProvider, RiftExchangeWebsocket};
use crate::evm_indexer::{broadcast_transaction, broadcast_transaction_via_flashbots};
use crate::Result;
use alloy::primitives::U256;
use alloy::providers::Provider;
use alloy::rpc::types::{Block, Transaction};
use futures::lock::Mutex;
use futures::StreamExt;
use log::info;
use std::fmt::Debug;
use std::sync::Arc;

#[derive(Debug)]
pub struct ReleaserRequestInput {
    reservation_id: U256,
    unlock_timestamp: u64,
}

impl ReleaserRequestInput {
    pub fn new(reservation_id: U256, unlock_timestamp: u64) -> Self {
        ReleaserRequestInput {
            reservation_id,
            unlock_timestamp,
        }
    }
}

pub struct ReleaserQueue {
    release_queue: Arc<Mutex<Vec<ReleaserRequestInput>>>,
    flashbots_provider: Arc<Option<EvmHttpProvider>>,
    contract: Arc<RiftExchangeWebsocket>,
    debug_url: String,
}

impl ReleaserQueue {
    pub fn new(
        flashbots_provider: Arc<Option<EvmHttpProvider>>,
        contract: Arc<RiftExchangeWebsocket>,
        debug_url: &str,
    ) -> Arc<Self> {
        let queue = Arc::new(Self {
            release_queue: Arc::new(Mutex::new(Vec::new())),
            flashbots_provider,
            contract,
            debug_url: debug_url.to_string(),
        });

        ReleaserQueue::trigger_release_on_blocks(Arc::clone(&queue)).unwrap();

        queue
    }

    pub async fn add(&self, req: ReleaserRequestInput) -> Result<()> {
        let mut release_queue_handle = self.release_queue.lock().await;
        release_queue_handle.push(req);
        Ok(())
    }

    fn trigger_release_on_blocks(queue: Arc<Self>) -> Result<()> {
        tokio::spawn(async move {
            let provider = queue.contract.provider();
            let sub = provider.subscribe_blocks().await.unwrap();

            let mut stream = sub.into_stream();
            while let Some(block) = stream.next().await {
                // Here you can add the logic to process the queue based on the new block
                match queue.process_queue(block).await {
                    Ok(_) => {}
                    Err(e) => {
                        log::error!("Error processing queue: {:?}", e);
                    }
                }
            }
        });
        Ok(())
    }

    async fn release_liquidity(&self, reservation_id: U256) -> Result<()> {
        let txn_calldata = self
            .contract
            .releaseLiquidity(reservation_id)
            .calldata()
            .to_owned();

        let tx_hash = if let Some(flashbots_provider) = self.flashbots_provider.as_ref() {
            info!(
                "Broadcasting release for reservation index: {} via Flashbots",
                reservation_id
            );
            broadcast_transaction_via_flashbots(&self.contract, flashbots_provider, &txn_calldata)
                .await?
        } else {
            broadcast_transaction(&self.contract, &txn_calldata, &self.debug_url).await?
        };
        info!("Liquidity released with evm tx hash: {}", tx_hash);
        Ok(())
    }

    async fn process_queue(&self, block: Block<Transaction>) -> Result<()> {
        // Implement the logic to process the queue based on the new block
        // This might involve checking if any reservations are now unlockable
        // and calling releaseLiquidity for those that are
        for releaser_request in self.release_queue.lock().await.iter() {
            if block.header.timestamp > releaser_request.unlock_timestamp {
                self.release_liquidity(releaser_request.reservation_id)
                    .await?;
            }
        }
        Ok(())
    }
}
