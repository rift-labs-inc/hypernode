use crate::constants::MAIN_ELF;
use crate::core::RiftExchange;
use crate::{
    btc_rpc::BitcoinRpcClient,
    core::{RiftExchange::RiftExchangeInstance, ThreadSafeStore},
};
use alloy::network::{EthereumWallet, NetworkWallet};
use alloy::primitives::{Address, FixedBytes, U256};
use alloy::providers::{Provider, ProviderBuilder, WalletProvider, WsConnect};
use alloy::rpc::types::TransactionRequest;
use alloy::signers::local::PrivateKeySigner;
use bitcoin::hex::DisplayHex;
use bitcoin::{hashes::Hash, opcodes::all::OP_RETURN, script::Builder, Block, Script};
use crypto_bigint::{AddMod, U256 as SP1OptimizedU256};
use eyre::Result;
use log::{error, info};
use rift_core::lp::LiquidityReservation;
use rift_lib;
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
        ws_rpc_url: String,
        rift_exchange_address: Address,
        private_key: [u8; 32],
    ) -> Self {
        let (sender, receiver) = mpsc::unbounded_channel();
        let queue = ProofBroadcastQueue { sender };
        tokio::spawn(ProofBroadcastQueue::consume_task(
            receiver,
            store,
            ws_rpc_url,
            rift_exchange_address,
            private_key,
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
        ws_rpc_url: String,
        rift_exchange_address: Address,
        private_key: [u8; 32],
    ) {
        let wallet =
            EthereumWallet::from(PrivateKeySigner::from_bytes(&private_key.into()).unwrap());
        let provider = ProviderBuilder::new()
            .with_recommended_fillers()
            .wallet(wallet)
            .on_ws(WsConnect::new(&ws_rpc_url))
            .await
            .expect("Failed to connect to WebSocket");

        let contract = RiftExchange::new(rift_exchange_address, provider.clone());

        while let Some(item) = receiver.recv().await {
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

            let txn_calldata = contract
                .proposeTransactionProof(
                    item.reservation_id,
                    bitcoin_tx_id.into(),
                    safe_block_height as u32,
                    proposed_block_height,
                    confirmation_block_height,
                    block_hashes,
                    solidity_proof.into(),
                )
                .calldata()
                .to_owned();

            println!("txn_calldata: {}", txn_calldata.as_hex());

            println!("Hypernode address: {}", provider.wallet().default_signer().address());

            let tx = TransactionRequest::default()
                .to(*contract.address())
                .input(txn_calldata.into());

            let tx_hash = provider.send_transaction(tx).await.unwrap();
            info!("Proof broadcasted with evm tx hash: {}", tx_hash.tx_hash());
        }
    }
}
