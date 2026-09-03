Subject: I built the Rust client Nozomi doesn't have, and found something in your tip accounting

Hi Cavey, hi Temporal team,

Ali Zeeshan here. I applied for Platform Engineer, Core Systems, and rather than wait on the résumé pile I built something for you.

nozomi-client (crates.io/crates/nozomi-client): a typed Rust client for Nozomi. All nine regions and both routes as enums, tip rotation across the 17 accounts, client-side rejection of under-minimum tips (which Nozomi drops silently), batch framing with your limits enforced before bytes leave, a keep-alive task tuned to your 65-second idle cutoff, no client-side resubmission because your docs say it lowers priority, tracing spans and counters on every send, and a verifier that reads what actually happened on-chain.

The verifier is what turned up the finding. Your docs say a reverted transaction still pays the tip. On-chain it doesn't: Solana rolls the tip transfer back with the failed instruction, and only the fee is charged. I checked balances on real reverts. Then I scanned ten minutes of traffic across all 17 tip accounts: 8,268 Nozomi-tipped transactions landed, 98.8% reverted. So Nozomi is paid on about 1 in 80 of the transactions it lands, and the rest burn leader blockspace for free. Full numbers and method: [FINDINGS link].

Revert protection is in your job posting. I'd like to be the person who builds it, along with the internal crates the posting leads with. I'm in India; your posting says exceptional fits get considered remote, and I'd rather let the work make that case than argue it.

zeeshan-site.vercel.app · github.com/alizeeshan1234/nozomi-rs

Ali
