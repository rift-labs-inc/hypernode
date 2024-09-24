use alloy::network::{Ethereum, EthereumWallet};
use alloy::primitives::U256;
use alloy::providers::fillers::{
    ChainIdFiller, FillProvider, GasFiller, JoinFill, NonceFiller, WalletFiller,
};
use alloy::providers::{Provider, RootProvider};
use alloy::pubsub::PubSubFrontend;
use alloy::sol;
use alloy::transports::http::Http;
use bitcoin::Block;
use reqwest::Client;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;

sol!(
    #[allow(missing_docs)]
    #[sol(rpc)]
    BlockHeaderAggregator,
    "data-aggregation-contracts/out/BlockHeaderAggregator.sol/BlockHeaderAggregator.json"
);

sol!(
    #[allow(missing_docs)]
    #[sol(rpc)]
    DepositVaultAggregator,
    "data-aggregation-contracts/out/DepositVaultsAggregator.sol/DepositVaultsAggregator.json"
);

sol! {
    struct BlockHashes {
        bytes[] hashes;
    }
}

sol!(
    #[allow(missing_docs)]
    #[sol(rpc)]
    RiftExchange,
    "artifacts/RiftExchange.json"
);

sol!(
    #[allow(missing_docs)]
    #[sol(rpc)]
    WETH,
    "artifacts/WETH.json"
);

sol!(
    #[allow(missing_docs)]
    #[sol(rpc)]
    IERC20,
    "artifacts/IERC20.json"
);

pub type EvmWebsocketProvider = FillProvider<
    JoinFill<
        JoinFill<
            JoinFill<JoinFill<alloy::providers::Identity, GasFiller>, NonceFiller>,
            ChainIdFiller,
        >,
        WalletFiller<EthereumWallet>,
    >,
    RootProvider<PubSubFrontend>,
    PubSubFrontend,
    Ethereum,
>;
pub type EvmHttpProvider = FillProvider<
    JoinFill<
        JoinFill<
            JoinFill<JoinFill<alloy::providers::Identity, GasFiller>, NonceFiller>,
            ChainIdFiller,
        >,
        WalletFiller<EthereumWallet>,
    >,
    RootProvider<Http<Client>>,
    Http<Client>,
    Ethereum,
>;

pub type RiftExchangeWebsocket =
    RiftExchange::RiftExchangeInstance<PubSubFrontend, Arc<EvmWebsocketProvider>>;
pub type RiftExchangeHttp = RiftExchange::RiftExchangeInstance<Http<Client>, Arc<EvmHttpProvider>>;

#[derive(Clone)]
pub struct BitcoinReservationFinalized {
    pub confirmation_height: u64,
    pub confirmation_block_hash: [u8; 32],
    pub safe_block_chainwork: [u8; 32],
    pub blocks: Vec<Block>,
    pub retarget_block: Block,
}

#[derive(Clone)]
pub struct BitcoinReservationInProgress {
    pub proposed_block_height: u64,
    pub proposed_block_hash: [u8; 32],
    pub txid: [u8; 32],
}

impl BitcoinReservationInProgress {
    pub fn new(proposed_block_height: u64, proposed_block_hash: [u8; 32], txid: [u8; 32]) -> Self {
        BitcoinReservationInProgress {
            proposed_block_height,
            proposed_block_hash,
            txid,
        }
    }
}

// stores data about the current state of a reservation, as well as the reservation itself
// metadata is used within the indexer to determine what to do with a reservation
#[derive(Clone)]
pub struct ReservationMetadata {
    pub reservation: RiftExchange::SwapReservation,
    pub reserved_vaults: Vec<RiftExchange::DepositVault>,
    pub btc_initial: Option<BitcoinReservationInProgress>,
    pub btc_final: Option<BitcoinReservationFinalized>,
    pub proof: Option<Vec<u8>>,
}

impl ReservationMetadata {
    pub fn new(
        reservation: RiftExchange::SwapReservation,
        reserved_vaults: Vec<RiftExchange::DepositVault>,
    ) -> Self {
        ReservationMetadata {
            reservation,
            reserved_vaults,
            btc_initial: None,
            btc_final: None,
            proof: None,
        }
    }
}

pub struct Store {
    pub reservations: HashMap<U256, ReservationMetadata>,
    // Cache available block hashes for building safe -> proposed -> confirmation chains
    pub safe_contract_block_hashes: HashMap<u64, [u8; 32]>,
}

impl Store {
    pub fn new() -> Self {
        Store {
            reservations: HashMap::new(),
            safe_contract_block_hashes: HashMap::new(),
        }
    }

    pub fn drop_expired_reservations(&mut self, current_timestamp: u64) {
        let stale_ids: Vec<U256> = self
            .reservations
            .iter()
            .filter(|&(_, metadata)| {
                (metadata.reservation.unlockTimestamp as u64) < current_timestamp
            })
            .map(|(&id, _)| id)
            .collect();

        for id in stale_ids {
            self.reservations.remove(&id);
        }
    }

    pub fn update_proof(&mut self, id: U256, proof: Vec<u8>) {
        let metadata = self.reservations.get_mut(&id).unwrap();
        metadata.proof = Some(proof);
    }

    pub fn update_btc_reservation_initial(
        &mut self,
        id: U256,
        proposed_block_height: u64,
        proposed_block_hash: [u8; 32],
        txid: [u8; 32],
    ) {
        let metadata = self.reservations.get_mut(&id).unwrap();
        metadata.btc_initial = Some(BitcoinReservationInProgress::new(
            proposed_block_height,
            proposed_block_hash,
            txid,
        ));
    }

    pub fn update_btc_reservation_final(
        &mut self,
        id: U256,
        confirmation_height: u64,
        confirmation_block_hash: [u8; 32],
        safe_block_chainwork: [u8; 32],
        blocks: Vec<Block>,
        retarget_block: Block,
    ) {
        let metadata = self.reservations.get_mut(&id).unwrap();
        metadata.btc_final = Some(BitcoinReservationFinalized {
            safe_block_chainwork,
            confirmation_height,
            confirmation_block_hash,
            blocks,
            retarget_block,
        });
    }

    pub fn insert(&mut self, swap_reservation_index: U256, reservation: ReservationMetadata) {
        self.reservations
            .insert(swap_reservation_index, reservation);
    }

    pub fn remove(&mut self, id: U256) {
        self.reservations.remove(&id);
    }

    pub fn get(&self, id: U256) -> Option<&ReservationMetadata> {
        self.reservations.get(&id)
    }
}

pub struct StoreGuard<'a> {
    guard: tokio::sync::MutexGuard<'a, Store>,
}

impl<'a> std::ops::Deref for StoreGuard<'a> {
    type Target = Store;

    fn deref(&self) -> &Self::Target {
        &self.guard
    }
}

impl<'a> std::ops::DerefMut for StoreGuard<'a> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.guard
    }
}

pub struct ThreadSafeStore(Arc<Mutex<Store>>);

impl ThreadSafeStore {
    pub fn new() -> Self {
        ThreadSafeStore(Arc::new(Mutex::new(Store::new())))
    }

    pub async fn with_lock<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&mut StoreGuard<'_>) -> R,
    {
        let guard = self.0.lock().await;
        let mut reservations_guard = StoreGuard { guard };
        f(&mut reservations_guard)
    }
}
