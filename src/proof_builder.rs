use alloy::primitives::U256;
use alloy::providers::Provider;
use bitcoin::hex::DisplayHex;
use bitcoin::{hashes::Hash, opcodes::all::OP_RETURN, script::Builder, Block, Script};
use eyre::Result;
use log::info;
use rift_core::lp::LiquidityReservation;
use rift_lib;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::time::{sleep, Duration};

use crate::constants::MAIN_ELF;
use crate::core::RiftExchange;
use crate::{
    btc_rpc::BitcoinRpcClient,
    core::{RiftExchange::RiftExchangeInstance, ThreadSafeStore},
};
use crypto_bigint::U256 as SP1OptimizedU256;

#[derive(Debug)]
pub struct ProofGenerationInput {
    reservation_id: U256,
}

impl ProofGenerationInput {
    pub fn new(reservation_id: U256) -> Self {
        ProofGenerationInput { reservation_id }
    }
}

pub struct ProofGenerationQueue {
    sender: mpsc::UnboundedSender<ProofGenerationInput>,
}

impl ProofGenerationQueue {
    pub fn new(store: Arc<ThreadSafeStore>) -> Self {
        let (sender, receiver) = mpsc::unbounded_channel();

        let queue = ProofGenerationQueue { sender };

        tokio::spawn(ProofGenerationQueue::consume_task(receiver, store));

        queue
    }

    pub fn add(&self, proof_args: ProofGenerationInput) {
        self.sender
            .send(proof_args)
            .expect("Failed to add to proof generation queue");
    }

    async fn consume_task(
        mut receiver: mpsc::UnboundedReceiver<ProofGenerationInput>,
        store: Arc<ThreadSafeStore>,
    ) {
        while let Some(item) = receiver.recv().await {
            let reservation_metadata = store
                .with_lock(|store| store.get(item.reservation_id).unwrap().clone())
                .await;
            let order_nonce = reservation_metadata.reservation.nonce.0.as_slice()[..32]
                .try_into()
                .unwrap();
            let reserved_vaults = reservation_metadata.reserved_vaults;
            let amounts_to_reserve = reservation_metadata.reservation.amountsToReserve;
            let liquidity_reservations = reserved_vaults
                .iter()
                .zip(amounts_to_reserve.iter())
                .map(|(vault, amount)| LiquidityReservation {
                    amount_reserved: SP1OptimizedU256::from_be_slice(&amount.to_be_bytes::<32>()),
                    btc_exchange_rate: vault.exchangeRate,
                    script_pub_key: *vault.btcPayoutLockingScript,
                })
                .collect::<Vec<_>>();

            println!("Liquidity reservations: {:?}", liquidity_reservations);

            let btc_final = reservation_metadata.btc_final.unwrap();
            let btc_initial = reservation_metadata.btc_initial.unwrap();
            let blocks = btc_final.blocks;

            let proposed_txid = btc_initial.txid;
            let proposed_block_index = btc_initial.proposed_block_height
                - blocks.first().unwrap().bip34_block_height().unwrap();
            let retarget_block = btc_final.retarget_block;

            // MID_TODO: Some kind of verification that the data between the reservation and the blocks is correct
            let circuit_input: rift_core::CircuitInput = rift_lib::proof::build_proof_input(
                order_nonce,
                &liquidity_reservations,
                &blocks,
                proposed_block_index as usize,
                &rift_lib::to_little_endian(proposed_txid),
                &retarget_block,
            );
            tokio::task::spawn_blocking(move || {
                info!(
                    "Execution report {:?}",
                    rift_lib::proof::execute(circuit_input, MAIN_ELF)
                );
                let solidity_proof =
                    rift_lib::proof::generate_plonk_proof(circuit_input, MAIN_ELF, Some(true));
                info!("Proof generated: {:?}", solidity_proof);
                // TODO: Send proof to the contract
            });

            info!(
                "Proof gen finished for reservation_id: {:?}",
                item.reservation_id
            );
        }
    }
}
