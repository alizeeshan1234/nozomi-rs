//! Send a pre-signed transaction (base64 in NOZOMI_TX) through Nozomi.
//!
//! NOZOMI_API_KEY=... NOZOMI_TX=<base64 signed tx> cargo run --example send
use nozomi_client::{Client, Region, Route};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter("nozomi_client=debug")
        .init();

    let key = std::env::var("NOZOMI_API_KEY")?;
    let tx_b64 = std::env::var("NOZOMI_TX")?;
    let tx = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, tx_b64)?;

    let client = Client::builder()
        .api_key(key)
        .region(Region::Auto)
        .route(Route::Cloudflare)
        .build()?;
    let _keepalive = client.spawn_keepalive();

    let floor = client.tip_floor().await?;
    println!("tip floor p50 = {} SOL", floor.landed_tips_50th_percentile);

    client.send(&tx).await?;
    println!("accepted; stats: {:?}", client.stats());
    Ok(())
}
