use alloy::primitives::U256;
use log::{error, info};
use rift_core::btc_light_client::AsLittleEndianBytes;
use rift_core::lp::LiquidityReservation;
use rift_lib;
use std::sync::Arc;
use tokio::sync::{mpsc, Semaphore};

use crate::constants::MAIN_ELF;
use crate::core::ThreadSafeStore;
use crate::error::HypernodeError;
use crate::proof_broadcast::{self, ProofBroadcastQueue};
use crate::{hyper_err, Result};
use crypto_bigint::U256 as SP1OptimizedU256;

pub fn buffer_to_18_decimals(amount: U256, token_decimals: u8) -> U256 {
    if token_decimals < 18 {
        amount * U256::from(10).pow(U256::from(18 - token_decimals))
    } else {
        amount
    }
}

pub fn unbuffer_from_18_decimals(amount: U256, token_decimals: u8) -> U256 {
    if token_decimals < 18 {
        amount / U256::from(10).pow(U256::from(18 - token_decimals))
    } else {
        amount
    }
}

pub fn wei_to_sats(wei_amount: U256, wei_sats_exchange_rate: U256) -> U256 {
    wei_amount / wei_sats_exchange_rate
}

pub fn sats_to_wei(sats_amount: U256, wei_sats_exchange_rate: U256) -> U256 {
    sats_amount * wei_sats_exchange_rate
}

#[derive(Debug, Clone)]
pub enum ProofGenerationInput {
    Transaction(TransactionProofInput),
    Block(BlockProofInput),
}

#[derive(Debug, Clone)]
pub struct TransactionProofInput {
    pub reservation_id: U256,
}

#[derive(Debug, Clone)]
pub struct BlockProofInput {
    pub safe_chainwork: U256,
    pub safe_block_height: u64,
    pub blocks: Vec<bitcoin::Block>,
    pub retarget_block: bitcoin::Block,
    pub retarget_block_height: u64,
}
pub struct ProofGenerationQueue {
    sender: mpsc::UnboundedSender<ProofGenerationInput>,
}

impl ProofGenerationQueue {
    pub fn new(
        store: Arc<ThreadSafeStore>,
        proof_broadcast_queue: Arc<ProofBroadcastQueue>,
        mock_proof_gen: bool,
        concurrency_limit: usize,
    ) -> Self {
        let (sender, receiver) = mpsc::unbounded_channel();

        let queue = ProofGenerationQueue { sender };

        tokio::spawn(ProofGenerationQueue::consume_task(
            receiver,
            store,
            proof_broadcast_queue,
            mock_proof_gen,
            concurrency_limit,
        ));

        queue
    }

    pub fn add(&self, proof_input: ProofGenerationInput) -> Result<()> {
        self.sender
            .send(proof_input)
            .map_err(|e| hyper_err!(Queue, "Failed to add to proof generation queue: {}", e))
    }

    async fn consume_task(
        mut receiver: mpsc::UnboundedReceiver<ProofGenerationInput>,
        store: Arc<ThreadSafeStore>,
        proof_broadcast_queue: Arc<ProofBroadcastQueue>,
        mock_proof_gen: bool,
        concurrency_limit: usize,
    ) {
        let semaphore = Arc::new(Semaphore::new(concurrency_limit));

        while let Some(item) = receiver.recv().await {
            match item {
                ProofGenerationInput::Block(block_input) => {
                    // Process block proofs immediately
                    tokio::spawn(Self::process_block_proof(
                        block_input,
                        store.clone(),
                        proof_broadcast_queue.clone(),
                        mock_proof_gen,
                    ));
                }
                ProofGenerationInput::Transaction(transaction_input) => {
                    let permit = match semaphore.clone().acquire_owned().await {
                        Ok(permit) => permit,
                        Err(e) => {
                            error!("Failed to acquire semaphore permit: {}", e);
                            continue;
                        }
                    };

                    let store_clone = store.clone();
                    let proof_broadcast_queue_clone = proof_broadcast_queue.clone();
                    let mock_proof_gen_clone = mock_proof_gen;

                    tokio::spawn(async move {
                        if let Err(e) = Self::process_transaction_proof(
                            transaction_input,
                            store_clone,
                            proof_broadcast_queue_clone,
                            mock_proof_gen_clone,
                        )
                        .await
                        {
                            error!("Error processing proof generation item: {}", e);
                        }
                        drop(permit);
                    });
                }
            }
        }
    }

    async fn process_item(
        item: ProofGenerationInput,
        store: Arc<ThreadSafeStore>,
        proof_broadcast_queue: Arc<ProofBroadcastQueue>,
        mock_proof_gen: bool,
        _semaphore: Arc<Semaphore>,
    ) -> Result<()> {
        match item {
            ProofGenerationInput::Transaction(transaction_input) => {
                Self::process_transaction_proof(
                    transaction_input,
                    store,
                    proof_broadcast_queue,
                    mock_proof_gen,
                )
                .await
            }
            ProofGenerationInput::Block(block_input) => {
                Self::process_block_proof(block_input, store, proof_broadcast_queue, mock_proof_gen)
                    .await
            }
        }
    }

    async fn process_transaction_proof(
        item: TransactionProofInput,
        store: Arc<ThreadSafeStore>,
        proof_broadcast_queue: Arc<ProofBroadcastQueue>,
        mock_proof_gen: bool,
    ) -> Result<()> {
        let reservation_metadata = store
            .with_lock(|store| store.get(item.reservation_id).cloned())
            .await
            .ok_or_else(|| hyper_err!(Store, "Reservation not found: {}", item.reservation_id))?;

        let order_nonce = reservation_metadata
            .reservation
            .nonce
            .0
            .as_slice()
            .get(..32)
            .and_then(|slice| slice.try_into().ok())
            .ok_or_else(|| hyper_err!(ProofGeneration, "Invalid order nonce"))?;

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

        let btc_final = reservation_metadata
            .btc_final
            .ok_or_else(|| hyper_err!(ProofGeneration, "BTC final data not found"))?;
        let btc_initial = reservation_metadata
            .btc_initial
            .ok_or_else(|| hyper_err!(ProofGeneration, "BTC initial data not found"))?;
        let blocks = btc_final.blocks;

        let proposed_txid = btc_initial.txid;
        let proposed_block_index = btc_initial.proposed_block_height - btc_final.safe_block_height;
        let retarget_block = btc_final.retarget_block;

        let circuit_input = rift_lib::proof::build_transaction_proof_input(
            order_nonce,
            &liquidity_reservations,
            SP1OptimizedU256::from_be_slice(&btc_final.safe_block_chainwork),
            btc_final.safe_block_height,
            &blocks,
            proposed_block_index as usize,
            &proposed_txid.to_little_endian(),
            &retarget_block,
            btc_final.retarget_block_height,
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
        .await
        .map_err(|e| hyper_err!(ProofGeneration, "Proof generation task panicked: {}", e))?;

        let (solidity_proof_bytes, public_values_string) = result;
        info!(
            "Proof generation for reservation_id: {:?} took: {:?}",
            item.reservation_id,
            proof_gen_timer.elapsed()
        );

        info!("Public Inputs Encoded: {:?}", public_values_string);

        let public_inputs = hex::decode(public_values_string.clone().trim_start_matches("0x"))
            .map_err(|e| hyper_err!(ProofGeneration, "Failed to decode public inputs: {}", e))?;

        store
            .with_lock(|store| {
                store.update_proof_data(item.reservation_id, solidity_proof_bytes, public_inputs)
            })
            .await;

        proof_broadcast_queue.add(proof_broadcast::ProofBroadcastInput::Transaction(
            proof_broadcast::TransactionBroadcastInput {
                reservation_id: item.reservation_id,
            },
        ))?;

        info!(
            "Finished processing reservation_id: {:?}",
            item.reservation_id
        );
        Ok(())
    }

    async fn process_block_proof(
        input: BlockProofInput,
        store: Arc<ThreadSafeStore>,
        proof_broadcast_queue: Arc<ProofBroadcastQueue>,
        mock_proof_gen: bool,
    ) -> Result<()> {
        let circuit_input = rift_lib::proof::build_block_proof_input(
            SP1OptimizedU256::from_be_slice(&input.safe_chainwork.to_be_bytes()),
            input.safe_block_height,
            &input.blocks,
            &input.retarget_block,
            input.retarget_block_height,
        );

        let proof_gen_timer = std::time::Instant::now();
        let result = tokio::task::spawn_blocking(move || {
            let (public_values_string, execution_report) =
                rift_lib::proof::execute(circuit_input, MAIN_ELF);
            info!(
                "Block proof executed with {} cycles",
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
        .await
        .map_err(|e| hyper_err!(ProofGeneration, "Proof generation task panicked: {}", e))?;

        let (solidity_proof_bytes, public_values_string) = result;
        info!("Block proof generated in {:?}", proof_gen_timer.elapsed());

        info!(
            "Block proof public inputs encoded: {:?}",
            public_values_string
        );

        let public_inputs = hex::decode(public_values_string.clone().trim_start_matches("0x"))
            .map_err(|e| hyper_err!(ProofGeneration, "Failed to decode public inputs: {}", e))?;

        // TODO: Broadcast block proof
        proof_broadcast_queue.add(proof_broadcast::ProofBroadcastInput::Block(
            proof_broadcast::BlockBroadcastInput {
                safe_chainwork: input.safe_chainwork,
                safe_block_height: input.safe_block_height,
                blocks: input.blocks,
                retarget_block: input.retarget_block,
                retarget_block_height: input.retarget_block_height,
                solidity_proof: solidity_proof_bytes,
                public_inputs,
            },
        ))?;

        info!("Finished generating block proof");
        Ok(())
    }
}
