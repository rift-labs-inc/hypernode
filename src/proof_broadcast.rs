use crate::constants::{CHALLENGE_PERIOD_MINUTES, MAIN_ELF};
use crate::core::{EvmHttpProvider, RiftExchange, RiftExchangeWebsocket};
use crate::{
    btc_rpc::BitcoinRpcClient,
    core::{RiftExchange::RiftExchangeInstance, ThreadSafeStore},
};
use alloy::network::eip2718::Encodable2718;
use alloy::network::{Ethereum, EthereumWallet, NetworkWallet, TransactionBuilder};
use alloy::primitives::{Address, FixedBytes, Uint, U256};
use alloy::providers::{Provider, ProviderBuilder, WalletProvider, WsConnect};
use alloy::rpc::types::{TransactionInput, TransactionRequest};
use alloy::signers::local::PrivateKeySigner;
use rift_core::btc_light_client::AsLittleEndianBytes;
use std::ops::Index;

use alloy::transports::http::Http;
use alloy::transports::BoxTransport;
use bitcoin::hex::DisplayHex;
use bitcoin::{hashes::Hash, opcodes::all::OP_RETURN, script::Builder, Block, Script};
use crypto_bigint::{AddMod, Encoding, U256 as SP1OptimizedU256};
use eyre::Result;
use log::{debug, error, info};
use rift_core::lp::LiquidityReservation;
use rift_lib::{self, AsRiftOptimizedBlock};
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::time::{sleep, Duration};

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

    pub fn add(&self, proof_args: ProofBroadcastInput) {
        self.sender
            .send(proof_args)
            .expect("Failed to add to proof broadcast queue");
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
            let contract = contract.clone();
            let provider = provider.clone();

            let flashbots_provider = flashbots_provider.clone();

            let reservation_metadata = store
                .with_lock(|store| store.get(item.reservation_id).unwrap().clone())
                .await;
            let solidity_proof = reservation_metadata.proof.unwrap();
            let btc_initial = reservation_metadata.btc_initial.unwrap();
            let btc_final = reservation_metadata.btc_final.unwrap();
            let mut bitcoin_tx_id = btc_initial.txid;
            bitcoin_tx_id.reverse();

            let proposed_block_height = btc_initial.proposed_block_height;
            let safe_block_height = btc_final
                .blocks
                .first()
                .unwrap()
                .bip34_block_height()
                .unwrap();
            let confirmation_block_height = btc_final
                .blocks
                .last()
                .unwrap()
                .bip34_block_height()
                .unwrap();
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
                    .map(|block| block.as_rift_optimized_block())
                    .collect::<Vec<_>>()
                    .as_slice(),
                SP1OptimizedU256::from_be_slice(&btc_final.safe_block_chainwork),
            )
            .iter()
            .map(|chainwork| Uint::<256, 4>::from_be_bytes(chainwork.to_be_bytes()))
            .collect::<Vec<_>>();

            let txn_calldata = contract
                .proposeTransactionProof(
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
                info!("Proposing proof using Flashbots");

                let tx = TransactionRequest::default()
                    .to(*contract.address())
                    .input(TransactionInput::new(txn_calldata));
                let tx = provider.fill(tx).await.unwrap();
                let tx_envelope = tx
                    .as_builder()
                    .unwrap()
                    .clone()
                    .build(&provider.wallet())
                    .await
                    .unwrap();
                let tx_encoded = tx_envelope.encoded_2718();
                let pending = provider
                    .send_raw_transaction(&tx_encoded)
                    .await
                    .unwrap()
                    .register()
                    .await
                    .unwrap();
                pending.tx_hash().to_owned()
            } else {
                let tx = TransactionRequest::default()
                    .to(*contract.address())
                    .input(TransactionInput::new(txn_calldata));

                provider
                    .send_transaction(tx)
                    .await
                    .unwrap()
                    .tx_hash()
                    .to_owned()
            };

            info!("Proof broadcasted with evm tx hash: {}", tx_hash);
        }
    }
}
