//! Scan recent Nozomi traffic from public chain data alone.
//!
//! Pulls the newest signatures touching every Nozomi tip account, then samples
//! full transactions to measure intended vs paid tips and fees burned on reverts.
//!
//! SOLANA_RPC_URL=https://... cargo run --example scan -- [sigs_per_account=200] [sample_txs=60]
use std::collections::BTreeMap;
use std::time::Duration;

use nozomi_client::{Verifier, TIP_ACCOUNTS};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let rpc = std::env::var("SOLANA_RPC_URL")
        .unwrap_or_else(|_| "https://api.mainnet-beta.solana.com".into());
    let mut args = std::env::args().skip(1);
    let per_account: usize = args.next().and_then(|s| s.parse().ok()).unwrap_or(200);
    let sample_n: usize = args.next().and_then(|s| s.parse().ok()).unwrap_or(60);
    let pause = Duration::from_millis(
        std::env::var("RPC_PAUSE_MS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(250),
    );

    let v = Verifier::new(rpc);
    let mut all = Vec::new();
    for acct in TIP_ACCOUNTS {
        match v.signatures_for(acct, per_account, None).await {
            Ok(sigs) => {
                eprintln!("{acct}: {} sigs", sigs.len());
                all.extend(sigs);
            }
            Err(e) => eprintln!("{acct}: error {e}"),
        }
        tokio::time::sleep(pause).await;
    }
    all.sort_by_key(|s| std::cmp::Reverse(s.slot));
    all.dedup_by(|a, b| a.signature == b.signature);

    let total = all.len();
    let reverted = all.iter().filter(|s| !s.succeeded()).count();
    let (min_slot, max_slot) = (
        all.iter().map(|s| s.slot).min().unwrap_or(0),
        all.iter().map(|s| s.slot).max().unwrap_or(0),
    );
    let (min_t, max_t) = (
        all.iter().filter_map(|s| s.block_time).min().unwrap_or(0),
        all.iter().filter_map(|s| s.block_time).max().unwrap_or(0),
    );

    println!("== Nozomi landing scan ==");
    println!(
        "transactions: {total}  (slots {min_slot}..{max_slot}, {}s window)",
        max_t - min_t
    );
    println!(
        "landed & succeeded: {}  ({:.1}%)",
        total - reverted,
        pct(total - reverted, total)
    );
    println!(
        "landed & reverted:  {}  ({:.1}%)",
        reverted,
        pct(reverted, total)
    );

    let mut per_slot: BTreeMap<u64, usize> = BTreeMap::new();
    for s in &all {
        *per_slot.entry(s.slot).or_default() += 1;
    }
    let busiest = per_slot
        .iter()
        .max_by_key(|(_, n)| **n)
        .map(|(s, n)| (*s, *n))
        .unwrap_or((0, 0));
    println!(
        "distinct slots: {}  busiest slot: {} with {} txs",
        per_slot.len(),
        busiest.0,
        busiest.1
    );

    // Sample full transactions, alternating success and revert so both are represented.
    let succ: Vec<_> = all
        .iter()
        .filter(|s| s.succeeded())
        .take(sample_n / 2)
        .collect();
    let rev: Vec<_> = all
        .iter()
        .filter(|s| !s.succeeded())
        .take(sample_n - succ.len())
        .collect();
    let mut intended_ok = 0u64;
    let mut paid_ok = 0u64;
    let mut intended_rev = 0u64;
    let mut paid_rev = 0u64;
    let mut fee_rev = 0u64;
    let mut n_ok = 0usize;
    let mut n_rev = 0usize;
    let mut tips_ok: Vec<u64> = Vec::new();
    for s in succ.iter().chain(rev.iter()) {
        match v.report(&s.signature, None).await {
            Ok(r) => {
                if r.succeeded {
                    n_ok += 1;
                    intended_ok += r.tip_intended_lamports;
                    paid_ok += r.tip_paid_lamports;
                    tips_ok.push(r.tip_paid_lamports);
                } else {
                    n_rev += 1;
                    intended_rev += r.tip_intended_lamports;
                    paid_rev += r.tip_paid_lamports;
                    fee_rev += r.fee_lamports;
                }
            }
            Err(e) => eprintln!("{}: {e}", s.signature),
        }
        tokio::time::sleep(pause).await;
    }
    tips_ok.sort_unstable();

    println!("\n== Sample of {} full transactions ==", n_ok + n_rev);
    println!(
        "succeeded ({n_ok}): intended tip {:.4} SOL, paid tip {:.4} SOL",
        sol(intended_ok),
        sol(paid_ok)
    );
    if !tips_ok.is_empty() {
        println!(
            "  paid tip p50 {:.4} SOL, p90 {:.4} SOL, max {:.4} SOL",
            sol(pctl(&tips_ok, 50)),
            sol(pctl(&tips_ok, 90)),
            sol(*tips_ok.last().unwrap())
        );
    }
    println!(
        "reverted ({n_rev}):  intended tip {:.4} SOL, paid tip {:.4} SOL, fees charged {:.6} SOL",
        sol(intended_rev),
        sol(paid_rev),
        sol(fee_rev)
    );
    if n_rev > 0 && paid_rev == 0 {
        println!("\nEvery sampled revert paid 0 tip. The tip is rolled back with the transaction; only fees are charged.");
    }
    Ok(())
}

fn pct(n: usize, d: usize) -> f64 {
    if d == 0 {
        0.0
    } else {
        n as f64 * 100.0 / d as f64
    }
}
fn sol(l: u64) -> f64 {
    l as f64 / 1e9
}
fn pctl(sorted: &[u64], p: usize) -> u64 {
    let idx = (sorted.len() * p / 100).min(sorted.len() - 1);
    sorted[idx]
}
