use dotenv::dotenv;
use clap::Parser;
use eyre::Result;
use hypernode::HypernodeArgs;


#[tokio::main]
async fn main() -> Result<()> {
    env_logger::init();
    dotenv().ok();
    let args = HypernodeArgs::parse();
    hypernode::node::run(args).await
}
