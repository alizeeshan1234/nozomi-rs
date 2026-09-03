//! Report on a landed transaction: slot, tip account, intended vs paid tip, revert status.
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
    if report.reverted_tip_refunded() {
        println!(
            "\nReverted on-chain. Tip of {} lamports was rolled back with the transaction; only the {} lamport fee was charged.",
            report.tip_intended_lamports, report.fee_lamports
        );
    } else if report.tip_charged() {
        println!(
            "\nLanded. Tip of {} lamports paid to {}.",
            report.tip_paid_lamports,
            report.tip_account.as_deref().unwrap_or("?")
        );
    } else if !report.went_through_nozomi() {
        println!("\nNo Nozomi tip instruction found; this transaction did not go through Nozomi.");
    }
    Ok(())
}
