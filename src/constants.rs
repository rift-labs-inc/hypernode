pub const RESERVATION_DURATION_HOURS: u64 = 8;
pub const HEADER_LOOKBACK_LIMIT: usize = 200;
// needs to be +1 than the value expected in the contract
pub const CONFIRMATION_HEIGHT_DELTA: u64 = 1;
pub const MAIN_ELF: &[u8] = include_bytes!("../circuits/elf/riscv32im-succinct-zkvm-elf");
