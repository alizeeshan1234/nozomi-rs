# nozomi-client

Rust client for [Nozomi](https://use.temporal.xyz/nozomi/readme.md), Temporal's Solana transaction landing service.

Nozomi's docs ship raw `reqwest` samples. This crate turns them into a library you can put in production: typed endpoints, tip validation, one warm connection, batch framing with the limits enforced, and a verifier that tells you what actually happened on-chain.

```toml
[dependencies]
nozomi-client = "0.1"
```

## What it fixes

| Footgun in the raw samples | What this crate does |
|---|---|
| Hardcodes one tip account | Rotates across all 17 published tip accounts per transaction, as the docs ask, to avoid write-lock contention |
| Under-tipped transactions are dropped **silently** with no error | `send_transaction` inspects the transaction and refuses to send without a tip ≥ 0.001 SOL |
| New HTTP client per request | One pooled client with TCP no-delay, plus a keep-alive task that pings `/ping` every 60 s (the server drops idle connections at 65 s) |
| Client-side retry loops | None. Nozomi retries for you and their docs say resubmitting lowers your priority. The client sends once and reports |
| Batch framing by hand | `frame_batch` does the big-endian length prefixes and enforces 16 tx / 1,232 B / 19,744 B limits before any bytes leave |
| API v2 returns nothing but 200 | `Verifier` looks the signature up on any Solana RPC: landed slot, tip account paid, tip size, and whether it reverted while still paying |

## Usage

```rust
use nozomi_client::{tip, Client, Region, Route, Verifier};

let client = Client::builder()
    .api_key(std::env::var("NOZOMI_API_KEY")?)
    .region(Region::Frankfurt)
    .route(Route::Direct { tls: false }) // lowest latency from a datacenter
    .build()?;
let _keepalive = client.spawn_keepalive();

// Size the tip from the live floor.
let floor = client.tip_floor().await?;
let lamports = floor.lamports(nozomi_client::tipfloor::Percentile::P75);

// Add the tip instruction to your transaction, sign it, then:
let sig = client.send_transaction(&signed_tx).await?; // checks the tip locally first

// Later, on any RPC:
let report = Verifier::new(rpc_url).report(&sig, Some(submitted_slot)).await?;
if report.tipped_but_reverted() {
    // landed, reverted on-chain, tip was still in the transaction
}
```

Byte-level API if you build transactions elsewhere: `send(&[u8])` (API v2), `send_rpc(&[u8])` (JSON-RPC, returns the signature), `send_batch(&[&[u8]])`.

## Features

- `solana` (default): `tip::tip_instruction`, `tip::inspect`, `Client::send_transaction`. Pulls in `solana-transaction`, `solana-message`, `solana-pubkey`, `solana-system-interface`.
- Without it the crate is `reqwest` + `serde` only and works on raw transaction bytes.

## Observability

Every send is a `tracing` span with `region`, `bytes`, and `tip_lamports`. `Client::stats()` exposes atomic counters: submitted, accepted, rejected locally, rejected by server, transport errors, and mean round trip.

## Endpoints

All nine regions with direct (`http`/`https`) and Cloudflare routes, plus `Region::Auto`. See `Region::base_url`.

## Status

0.1. API may change before 1.0. Not affiliated with Temporal.

## License

MIT OR Apache-2.0
