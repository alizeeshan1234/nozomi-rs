//! End-to-end demo of `nozomi-client`.
//!
//! What it does, in order:
//!
//! 1. Sizes the tip from the live tip stream (falls back to the REST floor,
//!    then to the minimum).
//! 2. Builds a real transaction: compute budget, a Nozomi tip to a random tip
//!    account, and a 1-lamport self-transfer as the "work".
//! 3. Signs it and inspects it locally, the same check the client runs before
//!    sending.
//! 4. Sends it through Nozomi over HTTP (API v2) or QUIC (HTTP/3).
//! 5. Verifies on-chain what happened: slot, tip intended vs tip paid, revert.
//!
//! Without `NOZOMI_API_KEY` it still does everything except land: it sends
//! with a placeholder key and shows the typed 401 you get back. Without
//! `FEE_PAYER` it signs with a throwaway keypair, which can never land because
//! it has no SOL, so that is also a dry run.
//!
//! ```text
//! NOZOMI_API_KEY=...            # your key; omit for a dry run
//! FEE_PAYER=~/.config/solana/id.json   # funded keypair; omit for a dry run
//! NOZOMI_REGION=frankfurt        # auto | pittsburgh | newark | ashburn | los-angeles |
//!                                # frankfurt | amsterdam | london | tokyo | singapore
//! NOZOMI_TRANSPORT=http          # http | quic
//! SOLANA_RPC_URL=https://...     # default: public mainnet RPC
//! PRIORITY_MICROLAMPORTS=10000   # compute-unit price for the priority fee
//! RUST_LOG=nozomi_client=debug   # to see the client's spans
//! cargo run -p nozomi-demo
//! ```

use std::str::FromStr;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context};
use base64::Engine;
use nozomi_client::tipfloor::Percentile;
use nozomi_client::{tip, Client, Error, QuicClient, Region, Route, Verifier, MIN_TIP_LAMPORTS};
use solana_keypair::Keypair;
use solana_message::{Message, VersionedMessage};
use solana_signer::Signer;
use solana_transaction::versioned::VersionedTransaction;

struct Config {
    api_key: Option<String>,
    region: Region,
    transport: String,
    rpc_url: String,
    fee_payer: Option<String>,
    priority_microlamports: u64,
}

impl Config {
    fn from_env() -> anyhow::Result<Self> {
        let region = match std::env::var("NOZOMI_REGION")
            .unwrap_or_else(|_| "auto".into())
            .to_lowercase()
            .as_str()
        {
            "auto" => Region::Auto,
            "pittsburgh" | "pit" => Region::Pittsburgh,
            "newark" | "ewr" => Region::Newark,
            "ashburn" | "ash" => Region::Ashburn,
            "los-angeles" | "lax" => Region::LosAngeles,
            "frankfurt" | "fra" => Region::Frankfurt,
            "amsterdam" | "ams" => Region::Amsterdam,
            "london" | "lon" => Region::London,
            "tokyo" | "tyo" => Region::Tokyo,
            "singapore" | "sgp" => Region::Singapore,
            other => return Err(anyhow!("unknown NOZOMI_REGION {other}")),
        };
        Ok(Self {
            api_key: std::env::var("NOZOMI_API_KEY")
                .ok()
                .filter(|k| !k.trim().is_empty()),
            region,
            transport: std::env::var("NOZOMI_TRANSPORT").unwrap_or_else(|_| "http".into()),
            rpc_url: std::env::var("SOLANA_RPC_URL")
                .unwrap_or_else(|_| "https://api.mainnet-beta.solana.com".into()),
            fee_payer: std::env::var("FEE_PAYER").ok(),
            priority_microlamports: std::env::var("PRIORITY_MICROLAMPORTS")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(10_000),
        })
    }
}

fn step(n: u8, what: &str) {
    println!("\n[{n}] {what}");
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "nozomi_client=info".into()),
        )
        .init();

    let cfg = Config::from_env()?;
    let dry_run = cfg.api_key.is_none() || cfg.fee_payer.is_none();
    println!("nozomi-demo");
    println!(
        "  region {} · transport {} · rpc {} · mode {}",
        cfg.region,
        cfg.transport,
        cfg.rpc_url,
        if dry_run {
            "DRY RUN (no key or no funded keypair)"
        } else {
            "LIVE"
        }
    );

    // ---------------------------------------------------------------- client
    let client = Client::builder()
        .api_key(
            cfg.api_key
                .clone()
                .unwrap_or_else(|| "dry-run-no-key".into()),
        )
        .region(cfg.region)
        // Cloudflare works from anywhere. From a datacenter, Direct { tls: false }
        // is lowest latency; see the Route docs for the TLS availability caveat.
        .route(Route::Cloudflare)
        .timeout(Duration::from_secs(5))
        .build()?;
    // Keeps one connection warm; the server drops idle ones after 65 s.
    let _keepalive = client.spawn_keepalive();
    println!("  sending to {}", client.base_url());

    // ------------------------------------------------------------------ tip
    step(1, "size the tip from the live tip stream");
    let tips = client.tip_watch();
    let mut w = tips.subscribe();
    let floor = tokio::time::timeout(Duration::from_secs(20), async {
        w.changed().await.ok();
        w.borrow().clone()
    })
    .await
    .ok()
    .flatten();
    let tip_lamports = match floor {
        Some(f) => {
            println!(
                "  stream frame at {}: p25 {:?} p50 {:?} p75 {:?} p95 {:?} SOL (complete: {})",
                f.time,
                f.landed_tips_25th_percentile,
                f.landed_tips_50th_percentile,
                f.landed_tips_75th_percentile,
                f.landed_tips_95th_percentile,
                f.is_complete()
            );
            f.lamports(Percentile::P75)
        }
        None => match client.tip_floor().await {
            Ok(f) => {
                println!(
                    "  no stream frame in 20 s; REST floor p75 {:?} SOL",
                    f.landed_tips_75th_percentile
                );
                f.lamports(Percentile::P75)
            }
            Err(e) => {
                println!("  tip floor unavailable ({e}); using the minimum");
                MIN_TIP_LAMPORTS
            }
        },
    };
    println!(
        "  tipping {} lamports ({:.6} SOL) at p75; minimum is {}",
        tip_lamports,
        tip_lamports as f64 / 1e9,
        MIN_TIP_LAMPORTS
    );

    // -------------------------------------------------------------- keypair
    step(2, "load the fee payer");
    let payer = match &cfg.fee_payer {
        Some(path) => {
            let path = shellexpand(path);
            let json = std::fs::read_to_string(&path).with_context(|| format!("read {path}"))?;
            let bytes: Vec<u8> = serde_json::from_str(&json).context("keypair json")?;
            Keypair::try_from(bytes.as_slice()).map_err(|e| anyhow!("keypair: {e}"))?
        }
        None => {
            println!("  FEE_PAYER not set; using a throwaway keypair (cannot land)");
            Keypair::new()
        }
    };
    println!("  payer {}", payer.pubkey());

    // ------------------------------------------------------------ blockhash
    step(3, "fetch a recent blockhash");
    let verifier = Verifier::new(cfg.rpc_url.clone());
    let blockhash = latest_blockhash(&cfg.rpc_url).await?;
    let submitted_slot = verifier.current_slot().await.ok();
    println!("  blockhash {blockhash} · slot {:?}", submitted_slot);

    // ----------------------------------------------------------- build + sign
    step(4, "build and sign the transaction");
    let tip_ix = tip::tip_instruction(&payer.pubkey(), tip_lamports);
    let tip_to = tip_ix.accounts[1].pubkey;
    let ixs = vec![
        solana_compute_budget_interface::ComputeBudgetInstruction::set_compute_unit_limit(20_000),
        solana_compute_budget_interface::ComputeBudgetInstruction::set_compute_unit_price(
            cfg.priority_microlamports,
        ),
        tip_ix,
        // The "work": a 1-lamport self transfer.
        solana_system_interface::instruction::transfer(&payer.pubkey(), &payer.pubkey(), 1),
    ];
    let message = Message::new_with_blockhash(&ixs, Some(&payer.pubkey()), &blockhash);
    let tx = VersionedTransaction::try_new(VersionedMessage::Legacy(message), &[&payer])
        .map_err(|e| anyhow!("sign: {e}"))?;
    let signature = tx.signatures[0].to_string();
    println!(
        "  tip account {tip_to} (one of {} rotated at random)",
        nozomi_client::TIP_ACCOUNTS.len()
    );
    println!("  signature {signature}");

    // -------------------------------------------------------------- inspect
    step(5, "inspect locally, as the client does before sending");
    let inspection = tip::inspect(&tx);
    for t in &inspection.tips {
        println!(
            "  instruction {} → {} : {} lamports",
            t.instruction_index, t.to, t.lamports
        );
    }
    match inspection.check() {
        Ok(()) => println!(
            "  check: ok, {} lamports total",
            inspection.total_lamports()
        ),
        Err(e) => println!("  check: would be refused: {e}"),
    }
    let bytes = tip::serialize(&tx)?;
    println!(
        "  {} bytes on the wire (limit {}) · base64 {}…",
        bytes.len(),
        nozomi_client::MAX_TX_BYTES,
        &base64::engine::general_purpose::STANDARD.encode(&bytes)[..24]
    );

    // ----------------------------------------------------------------- send
    step(6, &format!("send over {}", cfg.transport));
    let started = Instant::now();
    let outcome: Result<(), Error> = match cfg.transport.as_str() {
        "quic" => {
            let quic = QuicClient::builder()
                .api_key(
                    cfg.api_key
                        .clone()
                        .unwrap_or_else(|| "dry-run-no-key".into()),
                )
                .region(cfg.region)
                .timeout(Duration::from_secs(5))
                .build()?;
            match quic.warmup().await {
                Ok(()) => println!(
                    "  quic handshake to {} in {} ms",
                    quic.host(),
                    started.elapsed().as_millis()
                ),
                Err(e) => println!("  quic warmup to {} failed: {e}", quic.host()),
            }
            let r = quic.send(&bytes).await;
            println!("  quic stats: {:?}", quic.stats());
            r
        }
        _ => client.send_transaction(&tx).await.map(|_| ()),
    };
    let ms = started.elapsed().as_millis();
    match &outcome {
        Ok(()) => println!("  accepted by Nozomi in {ms} ms (200; no signature comes back, we computed it)"),
        Err(Error::Unauthorized) => println!("  401 Unauthorized in {ms} ms: the API key is missing or wrong (expected in a dry run)"),
        Err(Error::BadRequest(b)) => println!("  400 in {ms} ms: {b}"),
        Err(Error::RateLimited { retry_after }) => println!("  429 in {ms} ms, retry after {retry_after:?}"),
        Err(Error::Timeout { .. }) => println!("  timed out after {ms} ms; the server may still have it, so verify by signature rather than resend"),
        Err(e) => println!("  failed in {ms} ms: {e}"),
    }
    println!("  http stats: {:?}", client.stats());

    // --------------------------------------------------------------- verify
    step(7, "verify on-chain");
    if outcome.is_err() {
        println!("  nothing was accepted, so nothing to verify.");
        finish(dry_run);
        return Ok(());
    }
    let deadline = Instant::now() + Duration::from_secs(75);
    loop {
        match verifier.report(&signature, submitted_slot).await {
            Ok(report) => {
                println!("{}", serde_json::to_string_pretty(&report)?);
                if report.tip_charged() {
                    println!(
                        "  landed in slot {} ({:?} slots after submit); tip of {} lamports paid to {}",
                        report.slot,
                        report.slots_after_submit,
                        report.tip_paid_lamports,
                        report.tip_account.as_deref().unwrap_or("?")
                    );
                } else if report.reverted_tip_refunded() {
                    println!(
                        "  landed but reverted: {}. Tip of {} lamports rolled back; only the {} lamport fee was charged.",
                        report.error.as_deref().unwrap_or("?"),
                        report.tip_intended_lamports,
                        report.fee_lamports
                    );
                }
                break;
            }
            Err(Error::NotFound(_)) if Instant::now() < deadline => {
                print!(".");
                use std::io::Write;
                std::io::stdout().flush().ok();
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
            Err(Error::NotFound(_)) => {
                println!("\n  not found after 75 s: the blockhash expired without landing. Nozomi retried until then; nothing was charged.");
                break;
            }
            Err(e) => return Err(anyhow!("verify: {e}")),
        }
    }
    finish(dry_run);
    Ok(())
}

fn finish(dry_run: bool) {
    if dry_run {
        println!("\nDry run complete. Set NOZOMI_API_KEY and FEE_PAYER (a funded keypair) to land for real.");
    } else {
        println!("\nDone.");
    }
}

fn shellexpand(p: &str) -> String {
    match (p.strip_prefix("~/"), std::env::var("HOME")) {
        (Some(rest), Ok(home)) => format!("{home}/{rest}"),
        _ => p.to_string(),
    }
}

async fn latest_blockhash(rpc_url: &str) -> anyhow::Result<solana_hash::Hash> {
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()?;
    let v: serde_json::Value = http
        .post(rpc_url)
        .json(&serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "getLatestBlockhash",
            "params": [{ "commitment": "confirmed" }]
        }))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let s = v
        .pointer("/result/value/blockhash")
        .and_then(|b| b.as_str())
        .ok_or_else(|| anyhow!("getLatestBlockhash returned {v}"))?;
    solana_hash::Hash::from_str(s).map_err(|e| anyhow!("blockhash: {e}"))
}
