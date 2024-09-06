use std::sync::Arc;

use alloy::primitives::{U256};
use bitcoin::{hashes::Hash, opcodes::all::OP_RETURN, script::Builder, Block, Script};
use eyre::Result;

use crate::{btc_rpc::BitcoinRpcClient, core::ThreadSafeStore};



