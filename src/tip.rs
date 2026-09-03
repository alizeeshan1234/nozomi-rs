//! Nozomi tip accounts and tip-instruction helpers.
//!
//! Every Nozomi transaction must carry a plain system transfer to one of these
//! accounts. The docs ask you to pick a random one per transaction so that no
//! single account becomes a write-lock hotspot.

use rand::seq::SliceRandom;

/// Minimum tip Nozomi accepts, in lamports (0.001 SOL). Below this the transaction
/// is dropped without an error.
pub const MIN_TIP_LAMPORTS: u64 = 1_000_000;

/// The 17 public Nozomi tip accounts, as published in the docs.
pub const TIP_ACCOUNTS: [&str; 17] = [
    "TEMPaMeCRFAS9EKF53Jd6KpHxgL47uWLcpFArU1Fanq",
    "noz3jAjPiHuBPqiSPkkugaJDkJscPuRhYnSpbi8UvC4",
    "noz3str9KXfpKknefHji8L1mPgimezaiUyCHYMDv1GE",
    "noz6uoYCDijhu1V7cutCpwxNiSovEwLdRHPwmgCGDNo",
    "noz9EPNcT7WH6Sou3sr3GGjHQYVkN3DNirpbvDkv9YJ",
    "nozc5yT15LazbLTFVZzoNZCwjh3yUtW86LoUyqsBu4L",
    "nozFrhfnNGoyqwVuwPAW4aaGqempx4PU6g6D9CJMv7Z",
    "nozievPk7HyK1Rqy1MPJwVQ7qQg2QoJGyP71oeDwbsu",
    "noznbgwYnBLDHu8wcQVCEw6kDrXkPdKkydGJGNXGvL7",
    "nozNVWs5N8mgzuD3qigrCG2UoKxZttxzZ85pvAQVrbP",
    "nozpEGbwx4BcGp6pvEdAh1JoC2CQGZdU6HbNP1v2p6P",
    "nozrhjhkCr3zXT3BiT4WCodYCUFeQvcdUkM7MqhKqge",
    "nozrwQtWhEdrA6W8dkbt9gnUaMs52PdAv5byipnadq3",
    "nozUacTVWub3cL4mJmGCYjKZTnE9RbdY5AP46iQgbPJ",
    "nozWCyTPppJjRuw2fpzDhhWbW355fzosWSzrrMYB1Qk",
    "nozWNju6dY353eMkMqURqwQEoM3SFgEKC6psLCSfUne",
    "nozxNBgWohjR75vdspfxR5H9ceC7XXH99xpxhVGt3Bb",
];

/// Pick a random tip account, as the docs recommend for every transaction.
pub fn random_tip_account() -> &'static str {
    TIP_ACCOUNTS
        .choose(&mut rand::thread_rng())
        .copied()
        .expect("TIP_ACCOUNTS is non-empty")
}

/// True if `address` (base58) is one of the Nozomi tip accounts.
pub fn is_tip_account(address: &str) -> bool {
    TIP_ACCOUNTS.contains(&address)
}

/// A tip transfer found inside a transaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TipTransfer {
    /// Index of the instruction that carries the tip.
    pub instruction_index: usize,
    /// Destination tip account, base58.
    pub to: String,
    /// Lamports transferred.
    pub lamports: u64,
}

/// Result of scanning a transaction for tips.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TipInspection {
    pub tips: Vec<TipTransfer>,
}

impl TipInspection {
    /// Sum of all tip transfers found.
    pub fn total_lamports(&self) -> u64 {
        self.tips.iter().map(|t| t.lamports).sum()
    }

    /// Ok if the transaction carries at least the minimum tip, otherwise the
    /// error Nozomi would never send you.
    pub fn check(&self) -> crate::Result<()> {
        if self.tips.is_empty() {
            return Err(crate::Error::MissingTip);
        }
        let total = self.total_lamports();
        if total < MIN_TIP_LAMPORTS {
            return Err(crate::Error::TipBelowMinimum {
                lamports: total,
                minimum: MIN_TIP_LAMPORTS,
            });
        }
        Ok(())
    }
}

/// Decode a system-program `Transfer` instruction's data. Returns lamports if the
/// data is exactly a transfer (4-byte LE discriminator `2`, then u64 LE lamports).
pub fn decode_system_transfer(data: &[u8]) -> Option<u64> {
    if data.len() != 12 {
        return None;
    }
    let disc = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
    if disc != 2 {
        return None;
    }
    let mut buf = [0u8; 8];
    buf.copy_from_slice(&data[4..12]);
    Some(u64::from_le_bytes(buf))
}

#[cfg(feature = "solana")]
mod with_solana {
    use super::*;
    use solana_instruction::Instruction;
    use solana_pubkey::Pubkey;
    use solana_transaction::versioned::VersionedTransaction;
    use std::str::FromStr;
    use std::sync::OnceLock;

    /// The tip accounts as parsed `Pubkey`s.
    pub fn tip_pubkeys() -> &'static [Pubkey; 17] {
        static KEYS: OnceLock<[Pubkey; 17]> = OnceLock::new();
        KEYS.get_or_init(|| {
            let mut out = [Pubkey::default(); 17];
            for (i, s) in TIP_ACCOUNTS.iter().enumerate() {
                out[i] = Pubkey::from_str(s).expect("tip account is valid base58");
            }
            out
        })
    }

    /// A random tip account as a `Pubkey`.
    pub fn random_tip_pubkey() -> Pubkey {
        *tip_pubkeys()
            .choose(&mut rand::thread_rng())
            .expect("tip_pubkeys is non-empty")
    }

    /// Build the tip instruction: a system transfer of `lamports` from `payer`
    /// to a random Nozomi tip account. Add it anywhere in the transaction.
    pub fn tip_instruction(payer: &Pubkey, lamports: u64) -> Instruction {
        solana_system_interface::instruction::transfer(payer, &random_tip_pubkey(), lamports)
    }

    /// Like [`tip_instruction`] but to a specific tip account.
    pub fn tip_instruction_to(payer: &Pubkey, to: &Pubkey, lamports: u64) -> Instruction {
        solana_system_interface::instruction::transfer(payer, to, lamports)
    }

    /// Scan a transaction for system transfers to Nozomi tip accounts.
    ///
    /// Only static account keys are resolved. A tip whose destination comes from
    /// an address lookup table cannot be verified offline and is not counted.
    pub fn inspect(tx: &VersionedTransaction) -> TipInspection {
        let system_id = solana_system_interface::program::ID;
        let keys = tx.message.static_account_keys();
        let tips_set = tip_pubkeys();
        let mut tips = Vec::new();
        for (i, ix) in tx.message.instructions().iter().enumerate() {
            let Some(program) = keys.get(ix.program_id_index as usize) else {
                continue;
            };
            if *program != system_id {
                continue;
            }
            let Some(lamports) = decode_system_transfer(&ix.data) else {
                continue;
            };
            let Some(&to_idx) = ix.accounts.get(1) else {
                continue;
            };
            let Some(to) = keys.get(to_idx as usize) else {
                continue;
            };
            if tips_set.contains(to) {
                tips.push(TipTransfer {
                    instruction_index: i,
                    to: to.to_string(),
                    lamports,
                });
            }
        }
        TipInspection { tips }
    }

    /// Serialize a signed transaction to the wire bytes Nozomi expects.
    pub fn serialize(tx: &VersionedTransaction) -> crate::Result<Vec<u8>> {
        bincode::serialize(tx).map_err(|e| crate::Error::Serialize(e.to_string()))
    }
}

#[cfg(feature = "solana")]
pub use with_solana::*;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_tip_accounts_are_distinct_and_base58() {
        let mut set = std::collections::HashSet::new();
        for a in TIP_ACCOUNTS {
            assert!(set.insert(a), "duplicate tip account {a}");
            assert!(a.len() >= 32 && a.len() <= 44, "odd length for {a}");
            assert!(bs58_ok(a), "not base58: {a}");
        }
    }

    fn bs58_ok(s: &str) -> bool {
        const ALPHABET: &str = "123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz";
        s.chars().all(|c| ALPHABET.contains(c))
    }

    #[test]
    fn decodes_system_transfer() {
        let mut data = vec![2, 0, 0, 0];
        data.extend_from_slice(&1_000_000u64.to_le_bytes());
        assert_eq!(decode_system_transfer(&data), Some(1_000_000));
        assert_eq!(decode_system_transfer(&[0; 12]), None);
        assert_eq!(decode_system_transfer(&[2, 0, 0, 0]), None);
    }

    #[test]
    fn check_enforces_minimum() {
        let empty = TipInspection::default();
        assert!(matches!(empty.check(), Err(crate::Error::MissingTip)));
        let low = TipInspection {
            tips: vec![TipTransfer {
                instruction_index: 0,
                to: TIP_ACCOUNTS[0].into(),
                lamports: 999_999,
            }],
        };
        assert!(matches!(
            low.check(),
            Err(crate::Error::TipBelowMinimum {
                lamports: 999_999,
                ..
            })
        ));
        let ok = TipInspection {
            tips: vec![TipTransfer {
                instruction_index: 0,
                to: TIP_ACCOUNTS[0].into(),
                lamports: 1_000_000,
            }],
        };
        assert!(ok.check().is_ok());
    }

    #[cfg(feature = "solana")]
    #[test]
    fn inspect_finds_tip_in_built_transaction() {
        use solana_message::{Message, VersionedMessage};
        use solana_pubkey::Pubkey;
        use solana_transaction::versioned::VersionedTransaction;

        let payer = Pubkey::new_unique();
        let ix = tip_instruction(&payer, 2_000_000);
        let msg = Message::new(&[ix], Some(&payer));
        let tx = VersionedTransaction {
            signatures: vec![Default::default()],
            message: VersionedMessage::Legacy(msg),
        };
        let found = inspect(&tx);
        assert_eq!(found.tips.len(), 1);
        assert_eq!(found.total_lamports(), 2_000_000);
        assert!(is_tip_account(&found.tips[0].to));
        assert!(found.check().is_ok());
        let bytes = serialize(&tx).unwrap();
        assert!(bytes.len() >= crate::MIN_TX_BYTES && bytes.len() <= crate::MAX_TX_BYTES);
    }
}
