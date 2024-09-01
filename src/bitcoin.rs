use bitcoincore_rpc_async::{Auth, Client, RpcApi};
use eyre::Result;


async fn block_listener(rpc_url: &str) -> Result<()>{
    // user should encode user & pass into rpc url if needed
    let client = Client::new(rpc_url.to_string(), Auth::None).await?;

    loop {
        let block = client.wait_for_new_block(0).await?;
        todo!() // sift through block and update global reservation state 
    }

}

