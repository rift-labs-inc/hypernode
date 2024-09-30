use crate::core::ThreadSafeStore;
use crate::core::{EvmHttpProvider, RiftExchangeWebsocket};
use crate::{hyper_err, Result};
use crate::error::HypernodeError;
use alloy::network::eip2718::Encodable2718;
use alloy::network::TransactionBuilder;
use alloy::primitives::{FixedBytes, Uint, U256};
use alloy::providers::{Provider, WalletProvider};
use alloy::rpc::types::{TransactionInput, TransactionRequest};
use rift_core::btc_light_client::AsLittleEndianBytes;
use std::ops::Index;

use bitcoin::hashes::Hash;
use bitcoin::hex::DisplayHex;
use crypto_bigint::{Encoding, U256 as SP1OptimizedU256};
use log::{debug, error, info};
use rift_lib::{self, AsRiftOptimizedBlock};
use std::sync::Arc;
use tokio::sync::mpsc;

#[derive(Debug)]
pub struct ProofBroadcastInput {
    reservation_id: U256,
}

impl ProofBroadcastInput {
    pub fn new(reservation_id: U256) -> Self {
        ProofBroadcastInput { reservation_id }
    }
}

pub struct ProofBroadcastQueue {
    sender: mpsc::UnboundedSender<ProofBroadcastInput>,
}

impl ProofBroadcastQueue {
    pub fn new(
        store: Arc<ThreadSafeStore>,
        flashbots_provider: Arc<Option<EvmHttpProvider>>,
        contract: Arc<RiftExchangeWebsocket>,
    ) -> Self {
        let (sender, receiver) = mpsc::unbounded_channel();
        let queue = ProofBroadcastQueue { sender };
        tokio::spawn(ProofBroadcastQueue::consume_task(
            receiver,
            store,
            flashbots_provider,
            contract,
        ));
        queue
    }

    pub fn add(&self, proof_args: ProofBroadcastInput) -> Result<()> {
        self.sender
            .send(proof_args)
            .map_err(|e| hyper_err!(Queue, "Failed to add to proof broadcast queue: {}", e))
    }

    async fn consume_task(
        mut receiver: mpsc::UnboundedReceiver<ProofBroadcastInput>,
        store: Arc<ThreadSafeStore>,
        flashbots_provider: Arc<Option<EvmHttpProvider>>,
        contract: Arc<RiftExchangeWebsocket>,
    ) {
        let provider = contract.provider();

        info!(
            "Hypernode address: {}",
            provider.wallet().default_signer().address()
        );

        while let Some(item) = receiver.recv().await {
            if let Err(e) = Self::process_item(item, &store, &flashbots_provider, &contract).await {
                error!("Failed to process proof broadcast item: {}", e);
            }
        }
    }

    async fn process_item(
        item: ProofBroadcastInput,
        store: &Arc<ThreadSafeStore>,
        flashbots_provider: &Arc<Option<EvmHttpProvider>>,
        contract: &Arc<RiftExchangeWebsocket>,
    ) -> Result<()> {
        let reservation_metadata = store
            .with_lock(|store| store.get(item.reservation_id).cloned())
            .await
            .ok_or_else(|| hyper_err!(Store, "Reservation not found: {}", item.reservation_id))?;

        let solidity_proof = reservation_metadata.proof.ok_or_else(|| hyper_err!(ProofBroadcast, "Proof not found for reservation: {}", item.reservation_id))?;
        let btc_initial = reservation_metadata.btc_initial.ok_or_else(|| hyper_err!(ProofBroadcast, "BTC initial not found for reservation: {}", item.reservation_id))?;
        let btc_final = reservation_metadata.btc_final.ok_or_else(|| hyper_err!(ProofBroadcast, "BTC final not found for reservation: {}", item.reservation_id))?;
        
        let mut bitcoin_tx_id = btc_initial.txid;
        bitcoin_tx_id.reverse();

        let proposed_block_height = btc_initial.proposed_block_height;
        let safe_block_height = btc_final.safe_block_height;
        let confirmation_block_height = btc_final.confirmation_height;
        let block_hashes = btc_final
            .blocks
            .iter()
            .map(|block| {
                let mut block_hash = block.block_hash().to_raw_hash().to_byte_array();
                block_hash.reverse();
                FixedBytes::from_slice(&block_hash)
            })
            .collect::<Vec<_>>();

        let chainworks = rift_lib::transaction::get_chainworks(
            btc_final
                .blocks
                .iter()
                .zip(safe_block_height..safe_block_height + block_hashes.len() as u64)
                .map(|(block, height)| block.as_rift_optimized_block(height))
                .collect::<Vec<_>>()
                .as_slice(),
            SP1OptimizedU256::from_be_slice(&btc_final.safe_block_chainwork),
        )
        .iter()
        .map(|chainwork| Uint::<256, 4>::from_be_bytes(chainwork.to_be_bytes()))
        .collect::<Vec<_>>();

        let txn_calldata = contract
            .submitSwapProof(
                item.reservation_id,
                bitcoin_tx_id.into(),
                FixedBytes(
                    btc_final
                        .blocks
                        .index(
                            ((proposed_block_height as u64) - (safe_block_height as u64))
                                as usize,
                        )
                        .header
                        .merkle_root
                        .to_byte_array()
                        .to_little_endian(),
                ),
                safe_block_height as u32,
                proposed_block_height,
                confirmation_block_height,
                block_hashes,
                chainworks,
                solidity_proof.into(),
            )
            .calldata()
            .to_owned();

        debug!(
            "proposeTransactionProof calldata for reservation {} : {}",
            item.reservation_id,
            txn_calldata.as_hex()
        );

        let tx_hash = if flashbots_provider.is_some() {
            Self::propose_via_flashbots(contract, &txn_calldata).await?
        } else {
            Self::propose_standard(contract, &txn_calldata).await?
        };

        info!("Proof broadcasted with evm tx hash: {}", tx_hash);
        Ok(())
    }

    async fn propose_via_flashbots(
        contract: &Arc<RiftExchangeWebsocket>,
        txn_calldata: &[u8],
    ) -> Result<String> {
        info!("Proposing proof using Flashbots");
        let provider = contract.provider();
        let tx = TransactionRequest::default()
            .to(*contract.address())
            .input(TransactionInput::new(txn_calldata.to_vec().into()));
        
        let tx = provider.fill(tx).await
            .map_err(|e| hyper_err!(Evm, "Failed to fill transaction: {}", e))?;
        
        let tx_envelope = tx
            .as_builder()
            .unwrap()
            .clone()
            .build(&provider.wallet())
            .await
            .map_err(|e| hyper_err!(Evm, "Failed to build transaction envelope: {}", e))?;
        
        let tx_encoded = tx_envelope.encoded_2718();
        let pending = provider
            .send_raw_transaction(&tx_encoded)
            .await
            .map_err(|e| hyper_err!(Evm, "Failed to send raw transaction: {}", e))?
            .register()
            .await
            .map_err(|e| hyper_err!(Evm, "Failed to register transaction: {}", e))?;
        
        Ok(pending.tx_hash().to_string())
    }

    async fn propose_standard(
        contract: &Arc<RiftExchangeWebsocket>,
        txn_calldata: &[u8],
    ) -> Result<String> {
        let provider = contract.provider();
        let tx = TransactionRequest::default()
            .to(*contract.address())
            .input(TransactionInput::new(txn_calldata.to_vec().into()));

        match provider.send_transaction(tx.clone()).await {
            Ok(tx) => Ok(tx.tx_hash().to_string()),
            Err(e) => {
                let data = txn_calldata.as_hex();
                let to = contract.address().to_string();
                info!("cast call {} --data {} --trace", to, data);
                Err(hyper_err!(Evm, "Failed to broadcast proof: {}", e))
            }
        }
    }
}
