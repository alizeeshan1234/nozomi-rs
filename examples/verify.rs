//! Report on a landed transaction: slot, tip account, tip size, revert status.
//!
//! SOLANA_RPC_URL=https://... cargo run --example verify -- <signature> [submitted_slot]
use nozomi_client::Verifier;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let rpc = std::env::var("SOLANA_RPC_URL")
        .unwrap_or_else(|_| "https://api.mainnet-beta.solana.com".into());
    let mut args = std::env::args().skip(1);
    let sig = args
        .next()
        .expect("usage: verify <signature> [submitted_slot]");
    let submitted = args.next().and_then(|s| s.parse::<u64>().ok());

    let report = Verifier::new(rpc).report(&sig, submitted).await?;
    println!("{}", serde_json::to_string_pretty(&report)?);
    if report.tipped_but_reverted() {
        println!("\nWARNING: reverted on-chain but the tip instruction was in the transaction; Nozomi charges for this.");
    }
    Ok(())
}
