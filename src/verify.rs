//! Landing verification against any Solana RPC.
//!
//! Nozomi's API v2 returns nothing but a 200. This module answers the questions
//! that matter afterwards: did it land, in which slot, which tip account was
//! targeted, how much was *intended* as tip, and how much was *actually paid*.
//!
//! Those last two differ on a reverted transaction. Solana transactions are
//! atomic: if any instruction fails, every instruction is rolled back, including
//! the tip transfer. Only the base and priority fee are charged. Nozomi's docs
//! disagree with themselves here: the troubleshooting page says a reverted
//! transaction "still pays", the tipping FAQ says the tip "is never charged".
//! On-chain balances agree with the FAQ. This module reports both numbers so
//! you can see it yourself.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::error::Error;
use crate::tip::is_tip_account;

/// What the chain says about a Nozomi transaction.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LandingReport {
    pub signature: String,
    /// Slot the transaction was included in.
    pub slot: u64,
    /// Unix block time, if the RPC had it.
    pub block_time: Option<i64>,
    /// True if the transaction executed without error.
    pub succeeded: bool,
    /// The on-chain error, stringified, when `succeeded` is false.
    pub error: Option<String>,
    /// Base plus priority fee paid, in lamports. Charged even on revert.
    pub fee_lamports: u64,
    /// Compute units consumed, if reported.
    pub compute_units: Option<u64>,
    /// Nozomi tip account targeted by the tip instruction, if any.
    pub tip_account: Option<String>,
    /// Lamports the tip instruction(s) would transfer. What you *bid*.
    pub tip_intended_lamports: u64,
    /// Lamports that actually arrived at Nozomi tip accounts, from pre/post
    /// balances. What you *paid*. Zero on revert.
    pub tip_paid_lamports: u64,
    /// Slots between `submitted_slot` (if given) and inclusion.
    pub slots_after_submit: Option<u64>,
}

impl LandingReport {
    /// True if the transaction carried a tip instruction to a Nozomi account.
    pub fn went_through_nozomi(&self) -> bool {
        self.tip_intended_lamports > 0
    }

    /// True when the transaction reverted and, as Solana's atomicity requires,
    /// the tip was rolled back with it. You paid `fee_lamports` and nothing else.
    pub fn reverted_tip_refunded(&self) -> bool {
        !self.succeeded && self.tip_intended_lamports > 0 && self.tip_paid_lamports == 0
    }

    /// True when the tip was actually charged. Only happens on success.
    pub fn tip_charged(&self) -> bool {
        self.tip_paid_lamports > 0
    }

    /// True when a tip landed at a Nozomi account without a matching top-level
    /// instruction (for example via a lookup-table address or CPI this scan does
    /// not attribute). Worth a look if it ever happens.
    pub fn tip_paid_without_instruction(&self) -> bool {
        self.tip_paid_lamports > 0 && self.tip_intended_lamports == 0
    }
}

/// Looks up transactions on a Solana RPC. Uses `getTransaction` with
/// `jsonParsed` so it works without any Solana crates.
///
/// `Debug` output redacts the RPC URL, since hosted RPC URLs usually carry an
/// API key in the query string. Transport and decode errors have their URL
/// stripped for the same reason.
#[derive(Clone)]
pub struct Verifier {
    http: reqwest::Client,
    rpc_url: String,
    /// Known when this crate built the client; `None` for `with_client`.
    timeout: Option<std::time::Duration>,
}

impl std::fmt::Debug for Verifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Verifier")
            .field("rpc_url", &crate::util::REDACTED)
            .field("timeout", &self.timeout)
            .finish()
    }
}

/// Default per-request timeout for [`Verifier::new`].
pub const DEFAULT_RPC_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// One entry from `getSignaturesForAddress`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SignatureInfo {
    pub signature: String,
    pub slot: u64,
    #[serde(default)]
    pub err: Option<Value>,
    #[serde(default, rename = "blockTime")]
    pub block_time: Option<i64>,
}

impl SignatureInfo {
    pub fn succeeded(&self) -> bool {
        self.err.is_none()
    }
}

impl Verifier {
    /// Verifier against `rpc_url` with a 30-second per-request timeout.
    pub fn new(rpc_url: impl Into<String>) -> Self {
        Self::with_timeout(rpc_url, DEFAULT_RPC_TIMEOUT)
    }

    /// Verifier against `rpc_url` with a custom per-request timeout.
    pub fn with_timeout(rpc_url: impl Into<String>, timeout: std::time::Duration) -> Self {
        let http = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .expect("reqwest client with only a timeout set always builds");
        Self {
            http,
            rpc_url: rpc_url.into(),
            timeout: Some(timeout),
        }
    }

    /// Build from an existing client, to share a connection pool. Set a timeout
    /// on it yourself; this constructor does not add one, and a timeout it
    /// raises is reported as [`Error::Timeout`] with `after: None`.
    pub fn with_client(http: reqwest::Client, rpc_url: impl Into<String>) -> Self {
        Self {
            http,
            rpc_url: rpc_url.into(),
            timeout: None,
        }
    }

    async fn rpc(&self, method: &str, params: Value) -> crate::Result<Value> {
        let body = json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params });
        let resp = self
            .http
            .post(&self.rpc_url)
            .json(&body)
            .send()
            .await
            .map_err(|e| Error::from_reqwest(e, self.timeout))?;
        let status = resp.status();
        if !status.is_success() {
            return Err(Error::Http {
                status: status.as_u16(),
                body: resp.text().await.unwrap_or_default(),
            });
        }
        let v: Value = resp
            .json()
            .await
            .map_err(|e| Error::decode_reqwest(e, self.timeout))?;
        if let Some(err) = v.get("error") {
            return Err(Error::Rpc {
                code: err.get("code").and_then(|c| c.as_i64()).unwrap_or(0),
                message: err
                    .get("message")
                    .and_then(|m| m.as_str())
                    .unwrap_or("")
                    .to_string(),
            });
        }
        Ok(v.get("result").cloned().unwrap_or(Value::Null))
    }

    /// Current slot at `confirmed` commitment.
    pub async fn current_slot(&self) -> crate::Result<u64> {
        let v = self
            .rpc("getSlot", json!([{ "commitment": "confirmed" }]))
            .await?;
        v.as_u64()
            .ok_or_else(|| Error::Decode(format!("getSlot returned {v}")))
    }

    /// Recent signatures that touched `address`, newest first. `before` pages backwards.
    pub async fn signatures_for(
        &self,
        address: &str,
        limit: usize,
        before: Option<&str>,
    ) -> crate::Result<Vec<SignatureInfo>> {
        let mut opts = json!({ "limit": limit.min(1000), "commitment": "confirmed" });
        if let Some(b) = before {
            opts["before"] = json!(b);
        }
        let v = self
            .rpc("getSignaturesForAddress", json!([address, opts]))
            .await?;
        serde_json::from_value(v).map_err(|e| Error::Decode(format!("signatures: {e}")))
    }

    /// Fetch a confirmed transaction and report on it. `submitted_slot` is optional;
    /// pass the slot you observed just before sending to get `slots_after_submit`.
    pub async fn report(
        &self,
        signature: &str,
        submitted_slot: Option<u64>,
    ) -> crate::Result<LandingReport> {
        let params = json!([
            signature,
            { "encoding": "jsonParsed", "commitment": "confirmed", "maxSupportedTransactionVersion": 0 }
        ]);
        let result = self.rpc("getTransaction", params).await?;
        if result.is_null() {
            return Err(Error::NotFound(signature.to_string()));
        }
        Ok(parse_report(signature, &result, submitted_slot))
    }
}

/// Pure parser over a `getTransaction` (jsonParsed) result. Public so it can be
/// tested and reused on archived responses.
pub fn parse_report(signature: &str, result: &Value, submitted_slot: Option<u64>) -> LandingReport {
    let slot = result.get("slot").and_then(|s| s.as_u64()).unwrap_or(0);
    let block_time = result.get("blockTime").and_then(|t| t.as_i64());
    let meta = result.get("meta").cloned().unwrap_or(Value::Null);
    let err = meta
        .get("err")
        .filter(|e| !e.is_null())
        .map(|e| e.to_string());
    let fee_lamports = meta.get("fee").and_then(|f| f.as_u64()).unwrap_or(0);
    let compute_units = meta.get("computeUnitsConsumed").and_then(|c| c.as_u64());

    // Intended tip: system transfers to tip accounts in the instruction list.
    let mut tip_intended = 0u64;
    let mut tip_account = None;
    let mut visit = |ix: &Value| {
        let Some(parsed) = ix.get("parsed") else {
            return;
        };
        if ix.get("program").and_then(|p| p.as_str()) != Some("system") {
            return;
        }
        if parsed.get("type").and_then(|t| t.as_str()) != Some("transfer") {
            return;
        }
        let info = parsed.get("info").cloned().unwrap_or(Value::Null);
        let Some(dest) = info.get("destination").and_then(|d| d.as_str()) else {
            return;
        };
        if !is_tip_account(dest) {
            return;
        }
        tip_intended += info.get("lamports").and_then(|l| l.as_u64()).unwrap_or(0);
        if tip_account.is_none() {
            tip_account = Some(dest.to_string());
        }
    };
    if let Some(ixs) = result
        .pointer("/transaction/message/instructions")
        .and_then(|i| i.as_array())
    {
        for ix in ixs {
            visit(ix);
        }
    }
    if let Some(inner) = meta.get("innerInstructions").and_then(|i| i.as_array()) {
        for group in inner {
            if let Some(ixs) = group.get("instructions").and_then(|i| i.as_array()) {
                for ix in ixs {
                    visit(ix);
                }
            }
        }
    }

    // Paid tip: balance deltas on any tip account in the account list.
    let keys: Vec<String> = result
        .pointer("/transaction/message/accountKeys")
        .and_then(|k| k.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|k| {
                    k.get("pubkey")
                        .and_then(|p| p.as_str())
                        .or_else(|| k.as_str())
                        .map(str::to_string)
                })
                .collect()
        })
        .unwrap_or_default();
    let pre = meta
        .get("preBalances")
        .and_then(|b| b.as_array())
        .cloned()
        .unwrap_or_default();
    let post = meta
        .get("postBalances")
        .and_then(|b| b.as_array())
        .cloned()
        .unwrap_or_default();
    let mut tip_paid = 0u64;
    for (i, key) in keys.iter().enumerate() {
        if !is_tip_account(key) {
            continue;
        }
        let p0 = pre.get(i).and_then(|v| v.as_u64()).unwrap_or(0);
        let p1 = post.get(i).and_then(|v| v.as_u64()).unwrap_or(0);
        tip_paid += p1.saturating_sub(p0);
        if tip_account.is_none() && p1 > p0 {
            tip_account = Some(key.clone());
        }
    }

    LandingReport {
        signature: signature.to_string(),
        slot,
        block_time,
        succeeded: err.is_none(),
        error: err,
        fee_lamports,
        compute_units,
        tip_account,
        tip_intended_lamports: tip_intended,
        tip_paid_lamports: tip_paid,
        slots_after_submit: submitted_slot.map(|s| slot.saturating_sub(s)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(err: Value, tip_post: u64) -> Value {
        json!({
            "slot": 100,
            "blockTime": 1700000000,
            "meta": { "err": err, "fee": 5000, "computeUnitsConsumed": 1200, "innerInstructions": [],
                      "preBalances": [10_000_000, 500, 0], "postBalances": [10_000_000 - 5000 - (tip_post - 500), tip_post, 0] },
            "transaction": { "message": {
                "accountKeys": [ { "pubkey": "Payer111111111111111111111111111111111111111" },
                                 { "pubkey": crate::TIP_ACCOUNTS[3] },
                                 { "pubkey": "JUP6LkbZbjS1jKKwapdHNy74zcZ3tLUZoi5QNyVTaV4" } ],
                "instructions": [
                { "program": "system", "programId": "11111111111111111111111111111111",
                  "parsed": { "type": "transfer", "info": { "source": "Payer", "destination": crate::TIP_ACCOUNTS[3], "lamports": 1500000 } } },
                { "programId": "JUP6LkbZbjS1jKKwapdHNy74zcZ3tLUZoi5QNyVTaV4", "accounts": [], "data": "x" }
            ] } }
        })
    }

    #[test]
    fn success_pays_intended_tip() {
        let r = parse_report("sig", &sample(Value::Null, 500 + 1_500_000), Some(98));
        assert!(r.succeeded);
        assert_eq!(r.tip_intended_lamports, 1_500_000);
        assert_eq!(r.tip_paid_lamports, 1_500_000);
        assert!(r.tip_charged());
        assert!(!r.reverted_tip_refunded());
        assert_eq!(r.tip_account.as_deref(), Some(crate::TIP_ACCOUNTS[3]));
        assert_eq!(r.slots_after_submit, Some(2));
    }

    #[test]
    fn revert_rolls_back_tip() {
        // Balance on the tip account unchanged: the tip was intended but never paid.
        let r = parse_report(
            "sig",
            &sample(json!({ "InstructionError": [1, "Custom"] }), 500),
            None,
        );
        assert!(!r.succeeded);
        assert_eq!(r.tip_intended_lamports, 1_500_000);
        assert_eq!(r.tip_paid_lamports, 0);
        assert!(r.reverted_tip_refunded());
        assert!(!r.tip_charged());
        assert_eq!(r.fee_lamports, 5000);
    }

    #[test]
    fn no_tip_when_destination_is_not_nozomi() {
        let mut v = sample(Value::Null, 500);
        v["transaction"]["message"]["instructions"][0]["parsed"]["info"]["destination"] =
            json!("SomeoneElse111111111111111111111111111111111");
        v["transaction"]["message"]["accountKeys"][1]["pubkey"] =
            json!("SomeoneElse111111111111111111111111111111111");
        let r = parse_report("sig", &v, None);
        assert_eq!(r.tip_intended_lamports, 0);
        assert_eq!(r.tip_paid_lamports, 0);
        assert!(!r.went_through_nozomi());
    }
}
