use std::collections::HashMap;

use alloy::sol;
use RiftExchange::SwapReservation;

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

// stores data about the current state of a reservation, as well as the reservation itself
// metadata is used within the indexer to determine what to do with a reservation
pub struct ReservationMetadata {
    pub reservation: RiftExchange::SwapReservation,
    pub proposed_block_height: Option<u64>,
}

impl ReservationMetadata {
    fn new(reservation: RiftExchange::SwapReservation, proposed_block_height: Option<u64>) -> Self {
        ReservationMetadata {
            proposed_block_height,
            reservation,
        }
    }
}

pub struct ActiveReservations {
    pub reservations: HashMap<u64, ReservationMetadata>,
}

impl ActiveReservations {
    fn new() -> Self {
        ActiveReservations {
            reservations: HashMap::new(),
        }
    }

    fn drop_expired_reservations(&mut self, current_timestamp: u64) {
        let stale_ids: Vec<u64> = self
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
}
