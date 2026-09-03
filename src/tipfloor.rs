use serde::{Deserialize, Serialize};

use crate::error::Error;

/// Landed-tip percentiles from Nozomi's tip stream, in SOL as the API reports them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TipFloor {
    pub time: String,
    pub landed_tips_25th_percentile: f64,
    pub landed_tips_50th_percentile: f64,
    pub landed_tips_75th_percentile: f64,
    pub landed_tips_95th_percentile: f64,
    pub landed_tips_99th_percentile: f64,
}

/// Which percentile of recently landed tips to target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Percentile {
    P25,
    P50,
    P75,
    P95,
    P99,
}

impl TipFloor {
    /// Parse either a bare object or a one-element array, since the REST endpoint
    /// and the stream have both been seen to use each.
    pub fn from_value(v: &serde_json::Value) -> crate::Result<Self> {
        let obj = match v {
            serde_json::Value::Array(items) => items
                .first()
                .ok_or_else(|| Error::Decode("empty tip floor array".into()))?,
            other => other,
        };
        serde_json::from_value(obj.clone()).map_err(|e| Error::Decode(format!("tip floor: {e}")))
    }

    /// The tip at `p`, in SOL.
    pub fn sol(&self, p: Percentile) -> f64 {
        match p {
            Percentile::P25 => self.landed_tips_25th_percentile,
            Percentile::P50 => self.landed_tips_50th_percentile,
            Percentile::P75 => self.landed_tips_75th_percentile,
            Percentile::P95 => self.landed_tips_95th_percentile,
            Percentile::P99 => self.landed_tips_99th_percentile,
        }
    }

    /// The tip at `p` in lamports, never below the Nozomi minimum.
    pub fn lamports(&self, p: Percentile) -> u64 {
        let raw = (self.sol(p) * 1_000_000_000.0).round();
        let raw = if raw.is_finite() && raw > 0.0 {
            raw as u64
        } else {
            0
        };
        raw.max(crate::MIN_TIP_LAMPORTS)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_object_and_array() {
        let obj = serde_json::json!({
            "time": "2026-09-04T00:00:00Z",
            "landed_tips_25th_percentile": 0.001,
            "landed_tips_50th_percentile": 0.0015,
            "landed_tips_75th_percentile": 0.002,
            "landed_tips_95th_percentile": 0.01,
            "landed_tips_99th_percentile": 0.05
        });
        let a = TipFloor::from_value(&obj).unwrap();
        let b = TipFloor::from_value(&serde_json::json!([obj])).unwrap();
        assert_eq!(a, b);
        assert_eq!(a.lamports(Percentile::P50), 1_500_000);
        assert_eq!(a.lamports(Percentile::P25), 1_000_000);
    }

    #[test]
    fn lamports_never_below_minimum() {
        let f = TipFloor {
            time: String::new(),
            landed_tips_25th_percentile: 0.0000001,
            landed_tips_50th_percentile: 0.0,
            landed_tips_75th_percentile: -1.0,
            landed_tips_95th_percentile: 0.0,
            landed_tips_99th_percentile: 0.0,
        };
        for p in [Percentile::P25, Percentile::P50, Percentile::P75] {
            assert_eq!(f.lamports(p), crate::MIN_TIP_LAMPORTS);
        }
    }
}
