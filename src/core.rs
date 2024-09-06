use alloy::primitives::U256;
use alloy::sol;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;

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

#[derive(Clone)]
pub struct BitcoinReservationInProgress {
    pub proposed_block_height: u64,
    pub proposed_block_hash: [u8; 32],
    pub txid: [u8; 32],
    pub confirmation_height: Option<u64>,
    pub confirmation_block_hash: Option<[u8; 32]>
}
impl BitcoinReservationInProgress {
    pub fn new(
        proposed_block_height: u64,
        proposed_block_hash: [u8; 32],
        txid: [u8; 32],
    ) -> Self {
        BitcoinReservationInProgress {
            proposed_block_height,
            proposed_block_hash,
            txid,
            confirmation_height: None,
            confirmation_block_hash: None,
        }
    }
}

// stores data about the current state of a reservation, as well as the reservation itself
// metadata is used within the indexer to determine what to do with a reservation
#[derive(Clone)]
pub struct ReservationMetadata {
    pub reservation: RiftExchange::SwapReservation,
    pub btc: Option<BitcoinReservationInProgress>,
}

impl ReservationMetadata {
    pub fn new(
        reservation: RiftExchange::SwapReservation,
    ) -> Self {
        ReservationMetadata {
            reservation,
            btc: None,
        }
    }
}

pub struct ActiveReservations {
    pub reservations: HashMap<U256, ReservationMetadata>,
}

impl ActiveReservations {
    pub fn new() -> Self {
        ActiveReservations {
            reservations: HashMap::new(),
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

    pub fn update_btc_reservation(
        &mut self,
        id: U256,
        proposed_block_height: u64,
        proposed_block_hash: [u8; 32],
        txid: [u8; 32],
    ) {
        let metadata = self.reservations.get_mut(&id).unwrap();
        metadata.btc = Some(BitcoinReservationInProgress::new(
            proposed_block_height,
            proposed_block_hash,
            txid,
        ));
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

pub struct ActiveReservationsGuard<'a> {
    guard: tokio::sync::MutexGuard<'a, ActiveReservations>,
}

impl<'a> std::ops::Deref for ActiveReservationsGuard<'a> {
    type Target = ActiveReservations;

    fn deref(&self) -> &Self::Target {
        &self.guard
    }
}

impl<'a> std::ops::DerefMut for ActiveReservationsGuard<'a> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.guard
    }
}

pub struct SafeActiveReservations(Arc<Mutex<ActiveReservations>>);

impl SafeActiveReservations {
    pub fn new() -> Self {
        SafeActiveReservations(Arc::new(Mutex::new(ActiveReservations::new())))
    }

    pub async fn with_lock<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&mut ActiveReservationsGuard<'_>) -> R,
    {
        let guard = self.0.lock().await;
        let mut reservations_guard = ActiveReservationsGuard { guard };
        f(&mut reservations_guard)
    }
}
