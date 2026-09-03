# What the chain says about Nozomi traffic

Preliminary. Public mainnet RPC only, 2026-09-04. Numbers will be re-run on a paid RPC with a longer window before publishing.

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

Ten-minute window, slots 444054835 to 444056748, signatures on all 17 tip accounts (three accounts hit public-RPC rate limits, so this undercounts):

| | count | share |
|---|---|---|
| Nozomi-tipped transactions landed | 8,268 | |
| succeeded | 103 | 1.2% |
| reverted | 8,165 | 98.8% |

Rate: about 14 per second across 1,624 distinct slots, up to 39 in one slot.

Top revert reasons are program custom errors 1, 3, 13, 81, 7 and 111, which is the signature of sniper bots losing races: slippage, already-bought, pool-state checks. In the busiest slot sampled, 31 Nozomi-tipped transactions came from 11 payers, hitting pump.fun and a handful of sniper programs.

## 3. What that means in money and blockspace

Put 1 and 2 together. Nearly 99% of the transactions that carry a Nozomi tip revert, and reverted transactions pay Nozomi nothing. So:

- Nozomi is paid on roughly 1 in 80 of the transactions it is asked to land.
- The other 79 consume leader blockspace and compute (203k CU of Nozomi-tipped reverts in one sampled slot) and pay only the base fee, to the validator, not to Nozomi.
- Revert protection, meaning simulating before forwarding and dropping transactions that will fail, would cut forwarded volume by an order of magnitude with almost no revenue loss. It is the single largest lever in the pipeline, and it happens to be a line in Temporal's own job posting.

## 4. Live tip floor

`GET api.nozomi.temporal.xyz/tip_floor` on 2026-09-03: p25 0.0011 SOL, p50 0.005, p75 0.011, p95 0.085. The API returns a one-element array, not an object; the client parses both.

## Open questions for a paid-RPC run

- Tip distribution on successful transactions (public RPC rate-limited the sample to 10).
- How many of the 98.8% are the same payer retrying the same intent, i.e. double-lands.
- Whether the revert share differs by region or time of day.
