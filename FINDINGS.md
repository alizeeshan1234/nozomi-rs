# What the chain says about Nozomi traffic

Measured 2026-09-04 on mainnet via a paid RPC (Helius). Reproduce with `cargo run --example scan -- 3600 300`.

## Method

Every Nozomi transaction carries a system transfer to one of 17 published tip accounts. That makes the traffic observable from chain data alone: `getSignaturesForAddress` on each tip account, paged by block time, then `getTransaction` on a sample to read balances.

One caveat that applies to everything below: a transaction that *carries* a Nozomi tip is not proof it was *delivered* by Nozomi. Bots commonly attach several services' tips and broadcast through all of them. "Nozomi-tipped" is the honest label.

## 1. Reverted transactions do not pay the tip

The docs say: "a reverted transaction that includes the tip still pays."

On-chain, on a reverted Nozomi-tipped transaction (`47ZqgUey…`, slot 444055950):

| | lamports |
|---|---|
| tip instruction amount | 10,000,000 |
| tip account balance before | 890,880 |
| tip account balance after | 890,880 |
| payer balance change | −13,166 (the fee) |

Solana transactions are atomic. When instruction 4 failed, the tip transfer in instruction 0 was rolled back with it. The payer was charged the base and priority fee and nothing else. The docs conflate fee with tip. For a user this is good news; for anyone doing tip accounting it means "tip in transaction" and "tip received" are different columns.

`nozomi-client`'s `Verifier` reports both: `tip_intended_lamports` from the instruction, `tip_paid_lamports` from pre/post balances.

## 2. Almost all Nozomi-tipped transactions revert

Sixty-two-minute window, slots 444047263 to 444059106, every signature on all 17 tip accounts:

| | count | share |
|---|---|---|
| Nozomi-tipped transactions landed | 286,321 | |
| succeeded | 2,657 | 0.9% |
| reverted | 283,664 | 99.1% |

Rate: about 76 per second across 11,645 distinct slots, peaking at 247 in one slot.

Top revert reasons are program custom errors 3, 13, 81, 7, 6040 and 1, the signature of sniper bots losing races: slippage, already-bought, pool-state checks. In one busy slot inspected by hand, 31 Nozomi-tipped transactions came from 11 payers, mostly hitting pump.fun and a few sniper programs.

Tip sizes from a 300-transaction sample, half successes and half reverts:

| | successes (150) | reverts (150) |
|---|---|---|
| tip intended, total | 0.155 SOL | 12.93 SOL |
| tip paid, total | 0.155 SOL | 0 SOL |
| fees charged, total | | 0.0016 SOL |
| paid tip p50 / p90 / max | 0.0001 / 0.0015 / 0.039 SOL | |

Two things stand out. Reverting transactions carried on average 80 times the tip of successful ones: the bots that bid biggest are the ones losing the race. And half of the successful transactions paid under the documented 0.001 SOL minimum. Either the minimum is not enforced, or those transactions were landed by another service while carrying a small Nozomi tip. Both are worth knowing if you are doing tip accounting.

## 3. What that means in money and blockspace

Put 1 and 2 together. Nearly 99% of the transactions that carry a Nozomi tip revert, and reverted transactions pay Nozomi nothing. So:

- Nozomi is paid on roughly 1 in 100 of the transactions that carry its tip.
- The other 99 consume leader blockspace and compute (203k CU of Nozomi-tipped reverts in one sampled slot) and pay only the base fee, to the validator, not to Nozomi. In the sample, reverts carried 83 times more tip than successes and paid none of it.
- Revert protection, meaning simulating before forwarding and dropping transactions that will fail, would cut forwarded volume by an order of magnitude with almost no revenue loss. It is the single largest lever in the pipeline, and it happens to be a line in Temporal's own job posting.

## 4. Live tip floor

`GET api.nozomi.temporal.xyz/tip_floor` on 2026-09-03: p25 0.0011 SOL, p50 0.005, p75 0.011, p95 0.085. The API returns a one-element array, not an object; the client parses both.

## Open questions

- How many of the 99.1% are the same payer retrying the same intent, i.e. double-lands.
- Whether the revert share differs by time of day or around token launches.
- Whether sub-minimum tips are landing through Nozomi or elsewhere. Only Nozomi can answer that from its own logs.
