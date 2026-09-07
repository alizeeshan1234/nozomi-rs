# What the chain says about Nozomi traffic

Measured 2026-09-04 on mainnet via a paid RPC (Helius). Reproduce with `cargo run --example scan -- 3600 300`.

## Read this first

Everything below counts transactions that **carry a Nozomi tip instruction**. That is not the same as transactions **delivered by Nozomi**. Bots routinely attach tips for several landing services to one transaction and broadcast through all of them, so an unknown share of what follows was landed by someone else while carrying a Nozomi tip. Nothing here measures Nozomi's own landing rate; only Nozomi's logs can. "Nozomi-tipped" is the precise label and it is the one used throughout.

## Method

Every Nozomi transaction carries a system transfer to one of 17 published tip accounts. That makes the traffic observable from chain data alone: `getSignaturesForAddress` on each tip account, paged by block time, then `getTransaction` on a sample to read balances. Two consecutive windows were measured to check the numbers hold.

## 1. Reverted transactions do not pay the tip

The [troubleshooting page](https://use.temporal.xyz/nozomi/troubleshooting.md) says: "a reverted transaction that includes the tip still pays." The [tipping FAQ](https://use.temporal.xyz/nozomi/tipping-and-faq.md) says the opposite: "if the transaction fails, the tip is never charged." Checked 2026-09-07; both pages were live at once.

On-chain, on a reverted Nozomi-tipped transaction (`47ZqgUey…`, slot 444055950):

| | lamports |
|---|---|
| tip instruction amount | 10,000,000 |
| tip account balance before | 890,880 |
| tip account balance after | 890,880 |
| payer balance change | −13,166 (the fee) |

Solana transactions are atomic. When instruction 4 failed, the tip transfer in instruction 0 was rolled back with it. The payer was charged the base and priority fee and nothing else. The troubleshooting page conflates fee with tip; the FAQ has it right. For a user this is good news; for anyone doing tip accounting it means "tip in transaction" and "tip received" are different columns.

`nozomi-client`'s `Verifier` reports both: `tip_intended_lamports` from the instruction, `tip_paid_lamports` from pre/post balances.

## 2. Almost all Nozomi-tipped transactions revert

Two consecutive windows, every signature on all 17 tip accounts:

| | window 1 (62 min) | window 2 (21 min) |
|---|---|---|
| slots | 444047263 – 444059106 | 444059123 – 444063136 |
| Nozomi-tipped transactions landed | 286,321 | 132,603 |
| succeeded | 2,657 (0.9%) | 899 (0.7%) |
| reverted | 283,664 (99.1%) | 131,704 (99.3%) |
| rate | ~76 / s | ~104 / s |
| busiest slot | 247 txs | 325 txs |

Both windows fall in US afternoon hours, which is peak bot activity. The share should be re-measured at a quiet hour before anyone calls it a steady state.

Top revert reasons are program custom errors 3, 13, 81, 7, 6040 and 1, the signature of sniper bots losing races: slippage, already-bought, pool-state checks. In one busy slot inspected by hand, 31 Nozomi-tipped transactions came from 11 payers, mostly hitting pump.fun and a few sniper programs.

Tip sizes from two 300-transaction samples, each half successes and half reverts:

| | successes (150 + 150) | reverts (150 + 150) |
|---|---|---|
| tip paid, total | 0.155 SOL / 0.098 SOL | 0 SOL / 0 SOL |
| tip intended, total | 0.155 SOL / 0.098 SOL | 12.93 SOL / 0.82 SOL |
| fees charged, total | | 0.0016 SOL / 0.0019 SOL |
| tip p50 | 0.0001 SOL (paid) | 0.0020 SOL (intended, window 2) |
| tip p90 | 0.0015 / 0.0011 SOL | 0.0189 SOL (window 2) |
| tip max | 0.039 / 0.012 SOL | 0.090 SOL (window 2) |

Two things stand out. Reverting transactions carry larger tips than successful ones: about 20 times at the median, and much more at the mean because a few very large bids sit in the revert pile (the 12.93 SOL total in window 1 is dominated by a handful of outliers; window 2's 0.82 SOL is more typical). The bots that bid biggest are the ones losing the race. And half of the successful transactions paid under the documented 0.001 SOL minimum. Either the minimum is not enforced, or those transactions were landed by another service while carrying a small Nozomi tip. Both are worth knowing if you are doing tip accounting.

## 3. What that means in money and blockspace

Put 1 and 2 together. Nearly 99% of the transactions that carry a Nozomi tip revert, and reverted transactions pay Nozomi nothing. So:

- Nozomi is paid on roughly 1 in 100 of the transactions that carry its tip.
- The other 99 consume leader blockspace and compute (203k CU of Nozomi-tipped reverts in one sampled slot) and pay only the base fee, to the validator, not to Nozomi. At the median, reverts carried about 20 times the tip of successes and paid none of it.
- Revert protection, meaning simulating before forwarding and dropping transactions that will fail, would cut forwarded volume by an order of magnitude with almost no revenue loss. It is the single largest lever in the pipeline, and it happens to be a line in Temporal's own job posting.

## 4. Live tip floor

`GET api.nozomi.temporal.xyz/tip_floor` on 2026-09-03: p25 0.0011 SOL, p50 0.005, p75 0.011, p95 0.085. The API returns a one-element array, not an object; the client parses both.

Re-checked 2026-09-07 13:50 UTC: three requests eight seconds apart returned all-`null` percentiles, an nginx 503 page, and all-`null` again. The websocket `tip_stream` on the same host was pushing complete frames every 15 seconds at the same time (all five percentiles 0.0021 SOL, no API key needed). By 14:05 UTC the REST endpoint was answering normally again (p50 0.0011 SOL, all percentiles present). Anyone sizing tips from the REST endpoint needs a fallback; the client returns the minimum tip for a missing percentile and exposes `is_complete()`.

## 5. TLS and QUIC on the direct hosts are unreliable

The endpoints page lists nine direct hosts with `https://` and the QUIC client's README lists the same nine on UDP 443. Measured 2026-09-07 from one vantage point with `cargo run --example smoke --all-features` and a curl loop. Every host resolves to a single IPv4 address; `pit1` and `ewr1` share one, and `edge` (QUIC geo-DNS) resolved to `sgp1`'s address from here.

Ten TLS handshakes to `https://<host>/ping`, 14:30 UTC (each attempt is a fresh TCP connection; a failure is a reset at the ClientHello):

| host | TLS successes / 10 | QUIC (H3, 443), three runs 13:52 to 14:30 |
|---|---|---|
| ash1, lon1 | 10 | handshake OK, 401 without key, all three runs |
| sgp1 | 3 | OK all three runs |
| fra2 | 2 | OK at 13:52 and 14:05, refused at 14:30 |
| ams1 | 1 | refused all three runs |
| pit1, ewr1, lax1, tyo1 | 0 | refused all three runs |
| edge | | OK at 13:52 and 14:05, refused at 14:30 |
| plain `http://`, all nine | 10 | |
| Cloudflare hosts, all nine plus auto | 10 | |

So it is not a missing listener per host. TLS on 443 answers some connections and resets others, and the success rate differs by host and drifts over minutes; QUIC on the same hosts fails and recovers on the same schedule. That pattern fits a TLS-terminating front end that is rate-limiting or overloaded, or several backends behind one address with only some of them healthy. Plain HTTP on the same hosts, which the docs recommend from a datacenter anyway, never failed. Anyone using `Route::Direct { tls: true }` or QUIC should expect connect failures and keep the Cloudflare route or plain HTTP as a fallback; the QUIC client reconnects on its own, but a handshake that is refused stays refused until the host recovers.

## Open questions

- How many of the 99.1% are the same payer retrying the same intent, i.e. double-lands.
- Whether the revert share differs by time of day or around token launches.
- Whether sub-minimum tips are landing through Nozomi or elsewhere. Only Nozomi can answer that from its own logs.
