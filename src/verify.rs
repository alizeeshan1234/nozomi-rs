//! Landing verification against any Solana RPC.
//!
//! Nozomi's API v2 returns nothing but a 200. This module answers the questions
//! that matter afterwards: did it land, in which slot, which tip account got paid,
//! how much, and did the transaction revert while still paying the tip.

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
    /// Base fee paid, in lamports.
    pub fee_lamports: u64,
    /// Compute units consumed, if reported.
    pub compute_units: Option<u64>,
    /// Nozomi tip account that was paid, if any.
    pub tip_account: Option<String>,
    /// Total lamports transferred to Nozomi tip accounts.
    pub tip_lamports: u64,
    /// Slots between `submitted_slot` (if given) and inclusion.
    pub slots_after_submit: Option<u64>,
}

impl LandingReport {
    /// True when the transaction reverted but the tip transfer still executed.
    /// Nozomi charges in this case because the tip is an instruction in your own
    /// transaction. This is the case worth alerting on.
    pub fn tipped_but_reverted(&self) -> bool {
        !self.succeeded && self.tip_lamports > 0
    }

    /// True when the transaction landed with no tip to any Nozomi account, which
    /// means it did not go through Nozomi at all (or the tip was routed through a
    /// lookup table this scan cannot resolve).
    pub fn landed_without_tip(&self) -> bool {
        self.tip_lamports == 0
    }
}

/// Looks up transactions on a Solana RPC. Uses `getTransaction` with
/// `jsonParsed` so it works without any Solana crates.
#[derive(Debug, Clone)]
pub struct Verifier {
    http: reqwest::Client,
    rpc_url: String,
}

impl Verifier {
    pub fn new(rpc_url: impl Into<String>) -> Self {
        Self {
            http: reqwest::Client::new(),
            rpc_url: rpc_url.into(),
        }
    }

    /// Build from an existing client, to share a connection pool.
    pub fn with_client(http: reqwest::Client, rpc_url: impl Into<String>) -> Self {
        Self {
            http,
            rpc_url: rpc_url.into(),
        }
    }

    async fn rpc(&self, method: &str, params: Value) -> crate::Result<Value> {
        let body = json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params });
        let resp = self.http.post(&self.rpc_url).json(&body).send().await?;
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
            .map_err(|e| Error::Decode(e.to_string()))?;
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

    let mut tip_lamports = 0u64;
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
        let lamports = info.get("lamports").and_then(|l| l.as_u64()).unwrap_or(0);
        tip_lamports += lamports;
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

    // A reverted transaction still records its instructions, but only the tip
    // transfer "counts" if it actually moved lamports. On revert nothing moves,
    // yet Nozomi still charges by the docs' own description because the tip is
    // in the transaction. We report the *intended* tip so the caller can see what
    // was at stake, and `tipped_but_reverted` flags it.
    LandingReport {
        signature: signature.to_string(),
        slot,
        block_time,
        succeeded: err.is_none(),
        error: err,
        fee_lamports,
        compute_units,
        tip_account,
        tip_lamports,
        slots_after_submit: submitted_slot.map(|s| slot.saturating_sub(s)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(err: Value) -> Value {
        json!({
            "slot": 100,
            "blockTime": 1700000000,
            "meta": { "err": err, "fee": 5000, "computeUnitsConsumed": 1200, "innerInstructions": [] },
            "transaction": { "message": { "instructions": [
                { "program": "system", "programId": "11111111111111111111111111111111",
                  "parsed": { "type": "transfer", "info": { "source": "A", "destination": crate::TIP_ACCOUNTS[3], "lamports": 1500000 } } },
                { "programId": "JUP6LkbZbjS1jKKwapdHNy74zcZ3tLUZoi5QNyVTaV4", "accounts": [], "data": "x" }
            ] } }
        })
    }

    #[test]
    fn parses_success_with_tip() {
        let r = parse_report("sig", &sample(Value::Null), Some(98));
        assert!(r.succeeded);
        assert_eq!(r.tip_lamports, 1_500_000);
        assert_eq!(r.tip_account.as_deref(), Some(crate::TIP_ACCOUNTS[3]));
        assert_eq!(r.slots_after_submit, Some(2));
        assert_eq!(r.fee_lamports, 5000);
        assert_eq!(r.compute_units, Some(1200));
        assert!(!r.tipped_but_reverted());
    }

    #[test]
    fn flags_revert_with_tip() {
        let r = parse_report(
            "sig",
            &sample(json!({ "InstructionError": [1, "Custom"] })),
            None,
        );
        assert!(!r.succeeded);
        assert!(r.tipped_but_reverted());
        assert_eq!(r.slots_after_submit, None);
        assert!(r.error.unwrap().contains("InstructionError"));
    }

    #[test]
    fn no_tip_when_destination_is_not_nozomi() {
        let mut v = sample(Value::Null);
        v["transaction"]["message"]["instructions"][0]["parsed"]["info"]["destination"] =
            json!("SomeoneElse111111111111111111111111111111111");
        let r = parse_report("sig", &v, None);
        assert_eq!(r.tip_lamports, 0);
        assert!(r.landed_without_tip());
    }
}
