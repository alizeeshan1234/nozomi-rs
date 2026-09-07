# nozomi-demo

End-to-end demonstration of the [`nozomi-client`](https://crates.io/crates/nozomi-client) crate.

It sizes a tip from Nozomi's live tip stream, builds and signs a real transaction with a rotated tip account and a priority fee, inspects it the way the client does before sending, sends it through Nozomi over HTTP or QUIC, and then reads back from any Solana RPC what actually happened: the slot it landed in, the tip intended versus the tip paid, and whether it reverted.

Run from the repository root:

```sh
# dry run: no key, throwaway keypair. Does everything except land.
cargo run -p nozomi-demo

# live
export NOZOMI_API_KEY=...
export FEE_PAYER=~/.config/solana/id.json     # funded keypair
export NOZOMI_REGION=frankfurt                # default auto
export NOZOMI_TRANSPORT=quic                  # default http
export SOLANA_RPC_URL=https://...             # default public mainnet
cargo run -p nozomi-demo
```

`RUST_LOG=nozomi_client=debug` shows the client's own spans: region, bytes, tip lamports, and round-trip time per send.

What a live run costs: the base fee, the priority fee, and the tip (p75 of recently landed tips, typically 0.001 to 0.01 SOL). If the transaction reverts, the tip is rolled back and only the fees are charged; the verifier shows both numbers.

QUIC and TLS on Nozomi's direct hosts were intermittent when this was written (see section 5 of the crate's FINDINGS.md). If `NOZOMI_TRANSPORT=quic` reports "refused to accept a new connection", try another `NOZOMI_REGION` or fall back to `http`, which goes through Cloudflare and has not failed.
