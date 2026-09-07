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

/// A system transfer whose destination is loaded from an address lookup table.
/// It may or may not be a Nozomi tip; that cannot be decided without the table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnresolvedTransfer {
    /// Index of the instruction that carries the transfer.
    pub instruction_index: usize,
    /// Lamports transferred.
    pub lamports: u64,
}

/// Result of scanning a transaction for tips.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TipInspection {
    /// Transfers to a Nozomi tip account whose destination is a static key.
    pub tips: Vec<TipTransfer>,
    /// Transfers whose destination comes from an address lookup table. Not
    /// counted in [`total_lamports`](Self::total_lamports); see [`check`](Self::check).
    pub unresolved: Vec<UnresolvedTransfer>,
}

impl TipInspection {
    /// Sum of all resolved tip transfers found.
    pub fn total_lamports(&self) -> u64 {
        self.tips.iter().map(|t| t.lamports).sum()
    }

    /// Sum of transfers to lookup-table addresses. An upper bound on any tip
    /// this scan could not attribute.
    pub fn unresolved_lamports(&self) -> u64 {
        self.unresolved.iter().map(|t| t.lamports).sum()
    }

    /// Ok if the transaction carries at least the minimum tip, otherwise the
    /// error Nozomi would never send you.
    ///
    /// Only resolved tips count. If they fall short and there are unresolved
    /// transfers, the answer is [`Error::TipUnresolved`](crate::Error::TipUnresolved)
    /// rather than a guess either way: resolve the lookup-table addresses with
    /// [`inspect_with_loaded`] to get a definite answer.
    pub fn check(&self) -> crate::Result<()> {
        let resolved = self.total_lamports();
        if resolved >= MIN_TIP_LAMPORTS {
            return Ok(());
        }
        if !self.unresolved.is_empty() {
            return Err(crate::Error::TipUnresolved {
                resolved_lamports: resolved,
                unresolved_lamports: self.unresolved_lamports(),
            });
        }
        if self.tips.is_empty() {
            return Err(crate::Error::MissingTip);
        }
        Err(crate::Error::TipBelowMinimum {
            lamports: resolved,
            minimum: MIN_TIP_LAMPORTS,
        })
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
    /// Only static account keys are resolved. A transfer whose destination
    /// index points past the static keys, into addresses loaded from an
    /// address lookup table, is reported in
    /// [`TipInspection::unresolved`] rather than counted as a tip. Use
    /// [`inspect_with_loaded`] when you have the loaded addresses.
    pub fn inspect(tx: &VersionedTransaction) -> TipInspection {
        inspect_with_loaded(tx, &[])
    }

    /// Like [`inspect`], with the addresses the transaction loads from its
    /// lookup tables, so destinations there are resolved too. `loaded` is the
    /// writable loaded addresses followed by the readonly ones, in table order,
    /// which is how the runtime numbers them after the static keys. Indices
    /// past both lists are still reported as unresolved.
    pub fn inspect_with_loaded(tx: &VersionedTransaction, loaded: &[Pubkey]) -> TipInspection {
        let system_id = solana_system_interface::program::ID;
        let keys = tx.message.static_account_keys();
        let uses_lookups = tx
            .message
            .address_table_lookups()
            .is_some_and(|l| !l.is_empty());
        let tips_set = tip_pubkeys();
        let mut tips = Vec::new();
        let mut unresolved = Vec::new();
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
            let to_idx = to_idx as usize;
            let to = keys
                .get(to_idx)
                .or_else(|| loaded.get(to_idx.wrapping_sub(keys.len())));
            match to {
                Some(to) if tips_set.contains(to) => tips.push(TipTransfer {
                    instruction_index: i,
                    to: to.to_string(),
                    lamports,
                }),
                Some(_) => {}
                None if uses_lookups => unresolved.push(UnresolvedTransfer {
                    instruction_index: i,
                    lamports,
                }),
                None => {}
            }
        }
        TipInspection { tips, unresolved }
    }

    /// Serialize a signed transaction to the wire bytes Nozomi expects: the
    /// standard Solana encoding (short-vec signature count, signatures,
    /// message), byte-identical to `bincode::serialize` on the same value.
    pub fn serialize(tx: &VersionedTransaction) -> crate::Result<Vec<u8>> {
        wincode::serialize(tx).map_err(|e| crate::Error::Serialize(e.to_string()))
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
            ..Default::default()
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
            ..Default::default()
        };
        assert!(ok.check().is_ok());
    }

    #[test]
    fn unresolved_transfers_are_reported_not_guessed() {
        let via_lookup = TipInspection {
            tips: vec![],
            unresolved: vec![UnresolvedTransfer {
                instruction_index: 0,
                lamports: 1_000_000,
            }],
        };
        assert!(matches!(
            via_lookup.check(),
            Err(crate::Error::TipUnresolved {
                resolved_lamports: 0,
                unresolved_lamports: 1_000_000
            })
        ));
        // A real static tip that meets the minimum passes regardless.
        let enough = TipInspection {
            tips: vec![TipTransfer {
                instruction_index: 0,
                to: TIP_ACCOUNTS[0].into(),
                lamports: 1_000_000,
            }],
            unresolved: vec![UnresolvedTransfer {
                instruction_index: 1,
                lamports: 5,
            }],
        };
        assert!(enough.check().is_ok());
        // A short static tip plus an unresolved transfer is still unresolved.
        let short = TipInspection {
            tips: vec![TipTransfer {
                instruction_index: 0,
                to: TIP_ACCOUNTS[0].into(),
                lamports: 500_000,
            }],
            unresolved: vec![UnresolvedTransfer {
                instruction_index: 1,
                lamports: 1_000_000_000,
            }],
        };
        assert!(matches!(
            short.check(),
            Err(crate::Error::TipUnresolved {
                resolved_lamports: 500_000,
                ..
            })
        ));
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

    /// The wire format is fixed by the Solana runtime: a short-vec length,
    /// the signatures, then the serialized message. Anything else and Nozomi
    /// forwards garbage.
    #[cfg(feature = "solana")]
    #[test]
    fn serialize_is_the_solana_wire_format() {
        use solana_message::{Message, VersionedMessage};
        use solana_pubkey::Pubkey;
        use solana_transaction::versioned::VersionedTransaction;

        for (n_sigs, msg) in [
            (
                1usize,
                VersionedMessage::Legacy(Message::new(
                    &[tip_instruction(&Pubkey::new_unique(), 2_000_000)],
                    Some(&Pubkey::new_unique()),
                )),
            ),
            (1usize, v0_transfer_to_loaded_address(7).message),
        ] {
            let tx = VersionedTransaction {
                signatures: (0..n_sigs).map(|_| Default::default()).collect(),
                message: msg,
            };
            let mut expected = vec![n_sigs as u8]; // short-vec of a length < 128
            for s in &tx.signatures {
                expected.extend_from_slice(s.as_ref());
            }
            expected.extend_from_slice(&tx.message.serialize());
            assert_eq!(serialize(&tx).unwrap(), expected);
        }
    }

    /// A v0 transaction whose only system transfer goes to account index 2,
    /// the first writable address loaded from a lookup table.
    #[cfg(feature = "solana")]
    fn v0_transfer_to_loaded_address(
        lamports: u64,
    ) -> solana_transaction::versioned::VersionedTransaction {
        use solana_message::v0::{Message as V0Message, MessageAddressTableLookup};
        use solana_message::{MessageHeader, VersionedMessage};
        use solana_pubkey::Pubkey;
        use solana_transaction::versioned::VersionedTransaction;

        let payer = Pubkey::new_unique();
        let system = solana_system_interface::program::ID;
        let mut data = vec![2, 0, 0, 0];
        data.extend_from_slice(&lamports.to_le_bytes());
        let msg = V0Message {
            header: MessageHeader {
                num_required_signatures: 1,
                num_readonly_signed_accounts: 0,
                num_readonly_unsigned_accounts: 1,
            },
            account_keys: vec![payer, system],
            recent_blockhash: Default::default(),
            instructions: vec![solana_message::compiled_instruction::CompiledInstruction {
                program_id_index: 1,
                accounts: vec![0, 2],
                data,
            }],
            address_table_lookups: vec![MessageAddressTableLookup {
                account_key: Pubkey::new_unique(),
                writable_indexes: vec![0],
                readonly_indexes: vec![],
            }],
        };
        VersionedTransaction {
            signatures: vec![Default::default()],
            message: VersionedMessage::V0(msg),
        }
    }

    #[cfg(feature = "solana")]
    #[test]
    fn inspect_reports_lookup_table_destination_as_unresolved() {
        let tx = v0_transfer_to_loaded_address(1_500_000);
        let found = inspect(&tx);
        assert!(found.tips.is_empty());
        assert_eq!(found.unresolved.len(), 1);
        assert_eq!(found.unresolved_lamports(), 1_500_000);
        assert!(matches!(
            found.check(),
            Err(crate::Error::TipUnresolved {
                resolved_lamports: 0,
                unresolved_lamports: 1_500_000
            })
        ));
    }

    #[cfg(feature = "solana")]
    #[test]
    fn inspect_with_loaded_resolves_lookup_table_destination() {
        use solana_pubkey::Pubkey;
        let tx = v0_transfer_to_loaded_address(1_500_000);

        // The loaded address is a tip account: a real, checkable tip.
        let found = inspect_with_loaded(&tx, &[tip_pubkeys()[4]]);
        assert_eq!(found.tips.len(), 1);
        assert_eq!(found.tips[0].to, TIP_ACCOUNTS[4]);
        assert!(found.unresolved.is_empty());
        assert!(found.check().is_ok());

        // The loaded address is someone else: definitely no tip.
        let found = inspect_with_loaded(&tx, &[Pubkey::new_unique()]);
        assert!(found.tips.is_empty());
        assert!(found.unresolved.is_empty());
        assert!(matches!(found.check(), Err(crate::Error::MissingTip)));

        // Too few loaded addresses: still unresolved.
        let found = inspect_with_loaded(&tx, &[]);
        assert_eq!(found.unresolved.len(), 1);
    }
}
