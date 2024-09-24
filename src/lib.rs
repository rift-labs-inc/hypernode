pub mod btc_indexer;
pub mod btc_rpc;
pub mod constants;
pub mod core;
pub mod evm_indexer;
pub mod proof_broadcast;
pub mod proof_builder;
pub mod node;

use clap::Parser;

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
pub struct HypernodeArgs {
    /// Ethereum RPC websocket URL for indexing and proposing proofs onchain
    #[arg(short, long, env)]
    pub evm_ws_rpc: String,

    /// Bitcoin RPC URL for indexing
    #[arg(short, long, env)]
    pub btc_rpc: String,

    /// Ethereum private key for signing hypernode initiated transactions
    #[arg(short, long, env)]
    pub private_key: String,

    /// Rift Exchange contract address
    #[arg(short, long, env)]
    pub rift_exchange_address: String,

    /// RPC concurrency limit
    #[arg(short, long, env, default_value = "10")]
    pub rpc_concurrency: usize,

    /// Bitcoin new block polling interval in seconds
    #[arg(short, long, env, default_value = "30")]
    pub btc_polling_interval: u64,

    /// Enable mock proof generation
    #[arg(short, long, env, default_value = "false")]
    pub mock_proof: bool,

    /// Utilize Flashbots to prevent frontrunning on propose + release transactions (recommended
    /// for mainnet)
    #[arg(short, long, env, default_value = "false")]
    pub flashbots: bool,

    /// Flashbots relay URL, required if flashbots is enabled, will only be utilized when
    /// broadcasting transactions
    #[arg(short, long, env)]
    pub flashbots_relay_rpc: Option<String>,
}
