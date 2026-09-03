//! Scan recent Nozomi traffic from public chain data alone.
//!
//! Pulls the newest signatures touching every Nozomi tip account, then samples
//! full transactions to measure intended vs paid tips and fees burned on reverts.
//!
//! SOLANA_RPC_URL=https://... cargo run --example scan -- [window_secs=300] [sample_txs=60]
//!
//! Signatures are paged per tip account until `window_secs` of block time is
//! covered, so the landed/reverted split is over a real window rather than a
//! fixed count. Full-transaction sampling needs a paid RPC; public RPC rate limits it.
use std::collections::BTreeMap;
use std::time::Duration;

use nozomi_client::{Verifier, TIP_ACCOUNTS};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let rpc = std::env::var("SOLANA_RPC_URL")
        .unwrap_or_else(|_| "https://api.mainnet-beta.solana.com".into());
    let mut args = std::env::args().skip(1);
    let window_secs: i64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(300);
    let sample_n: usize = args.next().and_then(|s| s.parse().ok()).unwrap_or(60);
    let pause = Duration::from_millis(
        std::env::var("RPC_PAUSE_MS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(250),
    );

    let v = Verifier::new(rpc);
    let mut all = Vec::new();
    let mut newest_time: Option<i64> = None;
    for acct in TIP_ACCOUNTS {
        let mut before: Option<String> = None;
        let mut got = 0usize;
        loop {
            let page = match v.signatures_for(acct, 1000, before.as_deref()).await {
                Ok(p) => p,
                Err(e) => {
                    eprintln!("{acct}: error {e}");
                    break;
                }
            };
            tokio::time::sleep(pause).await;
            if page.is_empty() {
                break;
            }
            if newest_time.is_none() {
                newest_time = page.iter().filter_map(|s| s.block_time).max();
            }
            let cutoff = newest_time.unwrap_or(0) - window_secs;
            let oldest = page.iter().filter_map(|s| s.block_time).min().unwrap_or(0);
            let last_sig = page.last().map(|s| s.signature.clone());
            got += page.len();
            all.extend(
                page.into_iter()
                    .filter(|s| s.block_time.unwrap_or(0) >= cutoff),
            );
            if oldest < cutoff || got >= 20_000 {
                break;
            }
            before = last_sig;
        }
        eprintln!("{acct}: {got} sigs fetched");
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

    let mut errs: BTreeMap<String, usize> = BTreeMap::new();
    for s in all.iter().filter(|s| !s.succeeded()) {
        let key = s
            .err
            .as_ref()
            .map(|e| {
                // Collapse {"InstructionError":[idx,{"Custom":n}]} to "Custom(n)" so bursts show up.
                e.pointer("/InstructionError/1")
                    .map(|inner| inner.to_string())
                    .unwrap_or_else(|| e.to_string())
            })
            .unwrap_or_default();
        *errs.entry(key).or_default() += 1;
    }
    let mut errs: Vec<_> = errs.into_iter().collect();
    errs.sort_by_key(|(_, n)| std::cmp::Reverse(*n));
    println!("top revert reasons:");
    for (k, n) in errs.iter().take(6) {
        println!("  {:>6}  {k}", n);
    }

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
