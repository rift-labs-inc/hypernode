use alloy::primitives::U256;
use alloy::providers::Provider;
use bitcoin::hex::DisplayHex;
use bitcoin::{hashes::Hash, opcodes::all::OP_RETURN, script::Builder, Block, Script};
use eyre::Result;
use log::{error, info};
use rift_core::btc_light_client::AsLittleEndianBytes;
use rift_core::lp::LiquidityReservation;
use rift_lib;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::time::{sleep, Duration};

use crate::constants::MAIN_ELF;
use crate::core::RiftExchange;
use crate::proof_broadcast::{self, ProofBroadcastQueue};
use crate::{
    btc_rpc::BitcoinRpcClient,
    core::{RiftExchange::RiftExchangeInstance, ThreadSafeStore},
};
use crypto_bigint::{AddMod, Zero, U256 as SP1OptimizedU256};

fn buffer_to_18_decimals(amount: U256, token_decimals: u8) -> U256 {
    if token_decimals < 18 {
        amount * U256::from(10).pow(U256::from(18 - token_decimals))
    } else {
        amount
    }
}

fn unbuffer_from_18_decimals(amount: U256, token_decimals: u8) -> U256 {
    if token_decimals < 18 {
        amount / U256::from(10).pow(U256::from(18 - token_decimals))
    } else {
        amount
    }
}

fn wei_to_sats(wei_amount: U256, wei_sats_exchange_rate: U256) -> U256 {
    wei_amount / wei_sats_exchange_rate
}

fn sats_to_wei(sats_amount: U256, wei_sats_exchange_rate: U256) -> U256 {
    sats_amount * wei_sats_exchange_rate
}

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
    pub fn new(
        store: Arc<ThreadSafeStore>,
        proof_broadcast_queue: Arc<ProofBroadcastQueue>,
        mock_proof_gen: bool,
    ) -> Self {
        let (sender, receiver) = mpsc::unbounded_channel();

        let queue = ProofGenerationQueue { sender };

        tokio::spawn(ProofGenerationQueue::consume_task(
            receiver,
            store,
            proof_broadcast_queue,
            mock_proof_gen,
        ));

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
        proof_broadcast_queue: Arc<ProofBroadcastQueue>,
        mock_proof_gen: bool,
    ) {
        while let Some(item) = receiver.recv().await {
            let reservation_metadata = store
                .with_lock(|store| store.get(item.reservation_id).unwrap().clone())
                .await;
            let order_nonce = reservation_metadata.reservation.nonce.0.as_slice()[..32]
                .try_into()
                .unwrap();
            let reserved_vaults = reservation_metadata.reserved_vaults;
            let expected_sats_per_lp = reservation_metadata.reservation.expectedSatsOutput;
            let liquidity_reservations = reserved_vaults
                .iter()
                .zip(expected_sats_per_lp.iter())
                .map(|(vault, sats)| LiquidityReservation {
                    expected_sats: *sats,
                    script_pub_key: *vault.btcPayoutLockingScript,
                })
                .collect::<Vec<_>>();

            let btc_final = reservation_metadata.btc_final.unwrap();
            let btc_initial = reservation_metadata.btc_initial.unwrap();
            let blocks = btc_final.blocks;

            let proposed_txid = btc_initial.txid;
            let proposed_block_index = btc_initial.proposed_block_height
                - blocks.first().unwrap().bip34_block_height().unwrap();
            let retarget_block = btc_final.retarget_block;

            // TODO: Update hypernode to store the chainwork for a given safe block
            let circuit_input: rift_core::CircuitInput = rift_lib::proof::build_proof_input(
                order_nonce,
                &liquidity_reservations,
                SP1OptimizedU256::from_be_slice(&btc_final.safe_block_chainwork),
                &blocks,
                proposed_block_index as usize,
                &proposed_txid.to_little_endian(),
                &retarget_block,
            );
            let proof_gen_timer = std::time::Instant::now();
            let result = tokio::task::spawn_blocking(move || {
                let (public_values_string, execution_report) =
                    rift_lib::proof::execute(circuit_input, MAIN_ELF);
                info!(
                    "Reservation {} executed with {} cycles",
                    item.reservation_id,
                    execution_report.total_instruction_count()
                );
                if mock_proof_gen {
                    (Vec::new(), public_values_string)
                } else {
                    let proof =
                        rift_lib::proof::generate_plonk_proof(circuit_input, MAIN_ELF, Some(true));
                    let solidity_proof_bytes = proof.bytes();
                    (solidity_proof_bytes, public_values_string)
                }
            })
            .await;


            match result {
                Ok(proof) => {
                    let solidity_proof_bytes = proof.0;
                    info!(
                        "Proof generation for reservation_id: {:?} took: {:?}",
                        item.reservation_id,
                        proof_gen_timer.elapsed()
                    );
                    info!("Public Inputs Encoded: {:?}", proof.1);
                    // update the reservation with the proof
                    store
                        .with_lock(|store| {
                            store.update_proof(item.reservation_id, solidity_proof_bytes)
                        })
                        .await;

                    proof_broadcast_queue.add(proof_broadcast::ProofBroadcastInput::new(
                        item.reservation_id,
                    ));
                }
                Err(e) => {
                    error!(
                        "Error generating proof for reservation_id: {:?}, error: {:?}",
                        item.reservation_id, e
                    );
                }
            }
            info!(
                "Finished processing reservation_id: {:?}",
                item.reservation_id
            );
        }
    }
}
