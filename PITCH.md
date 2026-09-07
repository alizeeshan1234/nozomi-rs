Subject: I built the Rust client Nozomi doesn't have, and found something in your tip accounting

Hi Cavey, hi Temporal team,

Ali Zeeshan here. I applied for Platform Engineer, Core Systems, and rather than wait on the résumé pile I built something for you.

nozomi-client (crates.io/crates/nozomi-client): a typed Rust client for Nozomi. All nine regions and both routes as enums, tip rotation across the 17 accounts, client-side rejection of under-minimum tips (which Nozomi drops silently), batch framing with your limits enforced before bytes leave, a keep-alive task tuned to your 65-second idle cutoff, no client-side resubmission because your docs say it lowers priority, an HTTP/3 client for the QUIC path that reads the response (your reference client doesn't, so it can't see a 400), a reconnecting tip-stream subscription, tracing spans and counters on every send, and a verifier that reads what actually happened on-chain. Every network path has a mock-server test suite, and nothing ever logs an API key.

The verifier is what turned up the finding. Your troubleshooting page says a reverted transaction still pays the tip; your tipping FAQ says it never does. The chain agrees with the FAQ: Solana rolls the tip transfer back with the failed instruction, and only the fee is charged. I checked balances on real reverts. Then I scanned an hour of traffic across all 17 tip accounts: 286,321 Nozomi-tipped transactions landed, 99.1% reverted. So Nozomi is paid on about 1 in 100 of the transactions carrying its tip, and the rest burn leader blockspace for free. In a 300-transaction sample the reverts carried 20 times more tip than the successes at the median, 80 times at the mean, and paid none of it. Full numbers and method: github.com/alizeeshan1234/nozomi-rs/blob/main/FINDINGS.md

Two smaller things from the live smoke test on 7 September, in case they are news: TLS on 443 and QUIC on the direct hosts are intermittent, from 10/10 handshakes on ash1 and lon1 down to 0/10 on pit1, ewr1, lax1 and tyo1, with the rest in between and drifting over minutes, while plain HTTP and the Cloudflare hosts never failed; and /tip_floor was returning all-null percentiles and nginx 503s while the websocket stream on the same host was healthy. Section 5 of the findings has the table.

Revert protection is in your job posting. I'd like to be the person who builds it, along with the internal crates the posting leads with. I'm in India; your posting says exceptional fits get considered remote, and I'd rather let the work make that case than argue it.

zeeshan-site.vercel.app · github.com/alizeeshan1234/nozomi-rs

Ali
