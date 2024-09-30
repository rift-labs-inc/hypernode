use dotenv::dotenv;
use clap::Parser;
use hypernode::HypernodeArgs;
use hypernode::Result;

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::init();
    dotenv().ok();
    let args = HypernodeArgs::parse();
    hypernode::node::run(args).await
}
