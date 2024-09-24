use alloy::{
    network::{Ethereum, EthereumWallet},
    providers::{Provider, ProviderBuilder, WsConnect},
    pubsub::PubSubFrontend,
    signers::{k256::elliptic_curve::ff::derive::bitvec::boxed, local::PrivateKeySigner},
};
use dotenv;
use clap::Parser;
use eyre::Result;
use log::{info, trace, warn};
use std::{str::FromStr, sync::Arc};
use tokio::sync::Mutex;

use hypernode::constants::RESERVATION_DURATION_HOURS;
use hypernode::HypernodeArgs;
use hypernode::core::{
    EvmHttpProvider, EvmWebsocketProvider, RiftExchange, RiftExchangeHttp, RiftExchangeWebsocket,
    Store, ThreadSafeStore,
};
use hypernode::evm_indexer::fetch_token_decimals;
use hypernode::{evm_indexer, btc_indexer, proof_broadcast, proof_builder};


#[tokio::main]
async fn main() -> Result<()> {
    env_logger::init();
    dotenv::dotenv().ok();
    let args = HypernodeArgs::parse();
    hypernode::node::run(args).await
}
