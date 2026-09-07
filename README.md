# nozomi-client

Rust client for [Nozomi](https://use.temporal.xyz/nozomi/readme.md), Temporal's Solana transaction landing service.

Nozomi's docs ship raw `reqwest` samples. This crate turns them into a library you can put in production: typed endpoints, tip validation, one warm connection, batch framing with the limits enforced, and a verifier that tells you what actually happened on-chain.

```toml
[dependencies]
nozomi-client = "0.2"
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
| API key rides in the query string | `Debug` on the client redacts it, and transport errors have their URL stripped so a logged error never prints the key |
| Reference QUIC client never reads the response | `QuicClient` (feature `quic`) speaks HTTP/3 to the same batch endpoint, reads the status, and maps a 400 or 401 to the same typed errors as the HTTP client |
| Tip floor is a poll | `TipStream` (feature `tip-stream`) is the websocket feed behind it, one frame every 15 s, parsed into the same `TipFloor` |

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

The tip check resolves static account keys only. A transfer whose destination is loaded from an address lookup table comes back as `Error::TipUnresolved` rather than a guess. Pass the loaded addresses to `send_transaction_with_loaded` to check it exactly, or use `send_transaction_unchecked` to skip the check. `ClientBuilder::base_url` overrides the endpoint for proxies or tests.

## QUIC

For processes that cannot hold a warm TCP connection (serverless, frequent reconnects), Nozomi documents HTTP/3 over QUIC. Transport does not change priority; the tip does.

```rust
use nozomi_client::{QuicClient, Region};

let quic = QuicClient::builder().api_key(key).region(Region::Frankfurt).build()?;
quic.warmup().await?;                 // DNS + QUIC + H3 handshake now, not on the first send
quic.send(&signed_tx_bytes).await?;   // or send_batch, or send_raw with a pre-framed body
```

The connect runs single-flighted on its own task, so a burst of sends during an outage waits for one handshake instead of restarting it, and a caller's timeout never cancels it. A connection is dropped as soon as a send fails or times out on it. A send is retried once only if the failure happened before any request bytes left, so nothing is submitted twice. As of 2026-09-07 QUIC (and TLS on 443) on the direct hosts was intermittent: reliable from Ashburn and London, refused most of the time elsewhere, drifting over minutes. See FINDINGS.md section 5 and keep plain HTTP or the Cloudflare route as a fallback. `Region::Auto` uses `edge.nozomi.temporal.xyz`, which geo-routes.

## Tip stream

```rust
// Background subscription that reconnects on its own; read it at send time.
let tips = client.tip_watch();
let lamports = tips.latest_lamports(Percentile::P75); // minimum until the first frame

// Or drive the socket yourself.
let mut stream = client.tip_stream().await?;          // wss://api.nozomi.temporal.xyz/tip_stream
while let Some(floor) = stream.next().await? {
    tip.store(floor.lamports(Percentile::P75), Ordering::Relaxed);
}
```

One frame every 15 seconds. A connection that goes quiet for 45 seconds is treated as dead and replaced.

Percentiles are `Option<f64>`: the API returns all `null` when it has no recent landed tips, and `lamports()` then returns the minimum. Check `is_complete()` if you want to know.

## Features

- `solana` (default): `tip::tip_instruction`, `tip::inspect`, `Client::send_transaction`. Pulls in `solana-transaction`, `solana-message`, `solana-pubkey`, `solana-system-interface`.
- `quic`: `QuicClient`. Pulls in `quinn`, `h3`, `rustls` (ring), `webpki-roots`.
- `tip-stream`: `TipStream`, `Client::tip_stream`. Pulls in `tokio-tungstenite`.
- Without any of them the crate is `reqwest` + `serde` only and works on raw transaction bytes.

## Observability

Every send is a `tracing` span with `region` and `bytes`; `send_transaction` adds `tip_lamports`. `Client::stats()` exposes atomic counters: submitted, accepted, rejected locally, rejected by server, transport errors, and mean round trip.

## Testing

`cargo test --all-features` runs 62 tests. Beyond the unit tests there are mock-server suites for every network path: `tests/http.rs` (HTTP client: encoding, headers, the key parameter, status-to-error mapping, batch framing on the wire, ping, keep-alive task, tip floor), `tests/quic.rs` (a local HTTP/3 server with a self-signed cert: framing, status mapping, lazy connect, eviction of a stalled connection, recovery from a burst during an outage, timeout), `tests/tipstream.rs` (a local websocket server), and `tests/verify_http.rs` (a mock Solana RPC). Every suite asserts that no error ever prints an API key.

`cargo run --example smoke --all-features` hits the live service without a key: tip floor, `/ping` on all 28 region/route combinations, the three send endpoints (expect 401), QUIC to all ten hosts, and the tip stream. Set `NOZOMI_API_KEY` to send a malformed transaction through each path and confirm a typed 400.

## Endpoints

All nine regions with direct (`http`/`https`) and Cloudflare routes, plus `Region::Auto`. See `Region::base_url`.

## What the chain says about Nozomi traffic

See [FINDINGS.md](FINDINGS.md): over an hour of mainnet, 99.1% of Nozomi-tipped transactions reverted, and reverted transactions pay no tip. The docs say both things: the troubleshooting page says a revert "still pays", the tipping FAQ says the tip "is never charged". The chain agrees with the FAQ.

## Status

0.2. API may change before 1.0. Not affiliated with Temporal. See [CHANGELOG.md](CHANGELOG.md).

## License

MIT OR Apache-2.0, at your option. See `LICENSE-MIT` and `LICENSE-APACHE`.
