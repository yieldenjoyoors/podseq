//! Persisted force-inclusion inclusion markers for crash safety.
//!
//! Mirrors [`super::PendingStore`]: one JSON file under `data_dir`, written
//! atomically (tmp + rename), holding one record per inbox nonce. The
//! persist-before-settle rule lives here: a nonce is recorded `Submitted`
//! before its transaction enters the L2 pool and `Included` (with the block
//! height) before the settle call that covers it, so a crash at any point
//! re-derives state instead of double-including.

#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::StoreError;

/// State of one tracked inbox nonce.
///
/// The enum makes the invalid state unrepresentable: an included nonce carries
/// exactly one inclusion height, and a submitted nonce carries exactly the
/// deadline metadata needed to enforce it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum NonceRecord {
    /// Transaction entered the L2 pool; awaiting a block that contains it.
    Submitted {
        /// Deadline in L2 blocks: `first_seen_height + deadline_blocks`.
        deadline_height: u64,
        /// On-chain deadline (Sui clock ms) from `inbox::PendingTx`.
        deadline_ms: u64,
        /// Height at which the sequencer first observed the nonce.
        first_seen_height: u64,
        /// keccak256 of `tx_bytes`; correlates pool transactions back to the
        /// nonce during inclusion detection and crash recovery.
        tx_hash: String,
        /// Raw transaction bytes, re-sendable after a crash.
        tx_bytes: String,
    },
    /// Transaction landed in the L2 block at `height`. Terminal: a nonce can
    /// never go back to `Submitted`, and a second inclusion at a different
    /// height is rejected by [`InboxStore::mark_included`].
    Included {
        /// Height of the L2 block that contains the transaction.
        height: u64,
        /// keccak256 of the transaction bytes, kept so inclusion re-checks can
        /// trip the double-include rejection instead of passing silently.
        tx_hash: String,
    },
}

/// On-disk shape: `nonce -> record`, sorted by nonce.
#[derive(Debug, Default, Serialize, Deserialize)]
struct InboxFile {
    nonces: BTreeMap<u64, NonceRecord>,
}

/// Persistent store of force-inclusion inclusion markers at
/// `data_dir/inbox.json`.
pub struct InboxStore {
    path: PathBuf,
}

impl InboxStore {
    /// Creates a store at `data_dir/inbox.json`.
    pub fn new(data_dir: &Path) -> Self {
        Self {
            path: data_dir.join("inbox.json"),
        }
    }

    fn load_raw(&self) -> Result<InboxFile, StoreError> {
        if !self.path.exists() {
            return Ok(InboxFile::default());
        }
        let text = std::fs::read_to_string(&self.path)?;
        if text.trim().is_empty() {
            return Ok(InboxFile::default());
        }
        Ok(serde_json::from_str(&text)?)
    }

    fn save_raw(&self, file: &InboxFile) -> Result<(), StoreError> {
        let tmp = self.path.with_extension("json.tmp");
        let text = serde_json::to_string_pretty(file)?;
        std::fs::write(&tmp, &text)?;
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }

    fn mutate<F>(&self, f: F) -> Result<(), StoreError>
    where
        F: FnOnce(&mut BTreeMap<u64, NonceRecord>) -> Result<(), StoreError>,
    {
        let mut file = self.load_raw()?;
        f(&mut file.nonces)?;
        self.save_raw(&file)
    }

    /// Records `nonce` as submitted, before the transaction enters the pool.
    ///
    /// Errors when the nonce is already tracked: inbox nonces are assigned
    /// strictly increasingly, so a duplicate record is a caller bug.
    pub fn record_submitted(
        &self,
        nonce: u64,
        deadline_height: u64,
        deadline_ms: u64,
        first_seen_height: u64,
        tx_hash: String,
        tx_bytes: String,
    ) -> Result<(), StoreError> {
        self.mutate(|nonces| {
            if nonces.contains_key(&nonce) {
                return Err(StoreError::Conflict(format!(
                    "inbox nonce {nonce} is already tracked"
                )));
            }
            nonces.insert(
                nonce,
                NonceRecord::Submitted {
                    deadline_height,
                    deadline_ms,
                    first_seen_height,
                    tx_hash,
                    tx_bytes,
                },
            );
            Ok(())
        })
    }

    /// Marks `nonce` included in the block at `height`.
    ///
    /// Idempotent at the same height (a crash replay re-marks the same block);
    /// a different height means the same forced transaction landed twice,
    /// which must never be silently accepted, so it errors.
    pub fn mark_included(&self, nonce: u64, height: u64) -> Result<(), StoreError> {
        self.mutate(|nonces| {
            let Some(record) = nonces.get_mut(&nonce) else {
                return Err(StoreError::Conflict(format!(
                    "inbox nonce {nonce} was never tracked as submitted"
                )));
            };
            match record {
                NonceRecord::Included {
                    height: existing, ..
                } if *existing == height => Ok(()),
                NonceRecord::Included {
                    height: existing, ..
                } => Err(StoreError::Conflict(format!(
                    "inbox nonce {nonce} already included at height {existing}, \
                         refusing second inclusion at height {height}"
                ))),
                NonceRecord::Submitted { tx_hash, .. } => {
                    *record = NonceRecord::Included {
                        height,
                        tx_hash: tx_hash.clone(),
                    };
                    Ok(())
                }
            }
        })
    }

    /// Returns a copy of every tracked nonce record, keyed by nonce in
    /// ascending order.
    pub fn snapshot(&self) -> Result<BTreeMap<u64, NonceRecord>, StoreError> {
        Ok(self.load_raw()?.nonces)
    }

    /// Returns the record for `nonce`, if tracked.
    pub fn get(&self, nonce: u64) -> Result<Option<NonceRecord>, StoreError> {
        Ok(self.load_raw()?.nonces.get(&nonce).cloned())
    }

    /// Returns all submitted (not yet included) nonces in ascending order,
    /// with the metadata the poller needs to (re-)submit them.
    pub fn submitted(&self) -> Result<Vec<(u64, NonceRecord)>, StoreError> {
        Ok(self
            .load_raw()?
            .nonces
            .into_iter()
            .filter(|(_, record)| matches!(record, NonceRecord::Submitted { .. }))
            .collect())
    }

    /// Returns the count of nonces marked included: the sequencer's
    /// contribution to the settle cursor.
    pub fn included_count(&self) -> Result<u64, StoreError> {
        Ok(self
            .load_raw()?
            .nonces
            .values()
            .filter(|record| matches!(record, NonceRecord::Included { .. }))
            .count() as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_dir() -> PathBuf {
        // Process id + counter instead of a clock: tests run in parallel in
        // one process, and same-nanosecond collisions would share a data dir.
        use std::sync::atomic::{AtomicU64, Ordering};
        static DIR_COUNTER: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "podseq-inbox-store-test-{}-{}",
            std::process::id(),
            DIR_COUNTER.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn submitted_record() -> (&'static str, &'static str) {
        ("0xdeadbeef", "0x02f8aa0101")
    }

    #[test]
    fn record_submitted_then_get_returns_it() {
        let dir = tmp_dir();
        let store = InboxStore::new(&dir);
        let (hash, bytes) = submitted_record();

        store
            .record_submitted(3, 310, 1_600_000, 10, hash.into(), bytes.into())
            .unwrap();

        match store.get(3).unwrap() {
            Some(NonceRecord::Submitted {
                deadline_height,
                deadline_ms,
                first_seen_height,
                tx_hash,
                tx_bytes,
            }) => {
                assert_eq!(deadline_height, 310);
                assert_eq!(deadline_ms, 1_600_000);
                assert_eq!(first_seen_height, 10);
                assert_eq!(tx_hash, hash);
                assert_eq!(tx_bytes, bytes);
            }
            other => panic!("expected Submitted, got {other:?}"),
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn record_submitted_rejects_duplicate_nonce() {
        let dir = tmp_dir();
        let store = InboxStore::new(&dir);
        let (hash, bytes) = submitted_record();

        store
            .record_submitted(0, 100, 1, 0, hash.into(), bytes.into())
            .unwrap();
        assert!(store
            .record_submitted(0, 200, 2, 1, hash.into(), bytes.into())
            .is_err());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn mark_included_is_idempotent_at_same_height() {
        let dir = tmp_dir();
        let store = InboxStore::new(&dir);
        let (hash, bytes) = submitted_record();

        store
            .record_submitted(5, 300, 1, 0, hash.into(), bytes.into())
            .unwrap();
        store.mark_included(5, 42).unwrap();
        store.mark_included(5, 42).unwrap(); // crash replay

        assert_eq!(
            store.get(5).unwrap(),
            Some(NonceRecord::Included {
                height: 42,
                tx_hash: hash.to_string()
            })
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn mark_included_rejects_second_inclusion_at_different_height() {
        // The double-include tripwire: one forced transaction must appear in
        // exactly one block. A different-height re-mark means it landed twice.
        let dir = tmp_dir();
        let store = InboxStore::new(&dir);
        let (hash, bytes) = submitted_record();

        store
            .record_submitted(5, 300, 1, 0, hash.into(), bytes.into())
            .unwrap();
        store.mark_included(5, 42).unwrap();
        assert!(store.mark_included(5, 43).is_err());
        assert_eq!(
            store.get(5).unwrap(),
            Some(NonceRecord::Included {
                height: 42,
                tx_hash: hash.to_string()
            })
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn mark_included_rejects_untracked_nonce() {
        let dir = tmp_dir();
        let store = InboxStore::new(&dir);
        assert!(store.mark_included(9, 1).is_err());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn submitted_lists_only_unincluded_in_nonce_order() {
        let dir = tmp_dir();
        let store = InboxStore::new(&dir);
        let (hash, bytes) = submitted_record();

        store
            .record_submitted(1, 300, 1, 0, hash.into(), bytes.into())
            .unwrap();
        store
            .record_submitted(2, 300, 1, 0, hash.into(), bytes.into())
            .unwrap();
        store
            .record_submitted(3, 300, 1, 0, hash.into(), bytes.into())
            .unwrap();
        store.mark_included(2, 10).unwrap();

        let pending: Vec<u64> = store
            .submitted()
            .unwrap()
            .into_iter()
            .map(|(n, _)| n)
            .collect();
        assert_eq!(pending, vec![1, 3]);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn included_count_counts_included_only() {
        let dir = tmp_dir();
        let store = InboxStore::new(&dir);
        let (hash, bytes) = submitted_record();

        for nonce in 0..4 {
            store
                .record_submitted(nonce, 300, 1, 0, hash.into(), bytes.into())
                .unwrap();
        }
        assert_eq!(store.included_count().unwrap(), 0);
        store.mark_included(0, 5).unwrap();
        store.mark_included(1, 5).unwrap();
        assert_eq!(store.included_count().unwrap(), 2);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn survives_reload_from_disk() {
        // Crash simulation: a fresh store over the same data dir must see the
        // same markers, and an included nonce must stay excluded from the
        // re-submission set (the idempotent-marker guarantee).
        let dir = tmp_dir();
        let (hash, bytes) = submitted_record();
        {
            let store = InboxStore::new(&dir);
            store
                .record_submitted(7, 300, 1, 0, hash.into(), bytes.into())
                .unwrap();
            store
                .record_submitted(8, 300, 1, 0, hash.into(), bytes.into())
                .unwrap();
            store.mark_included(7, 12).unwrap();
        }
        let reloaded = InboxStore::new(&dir);
        assert_eq!(
            reloaded.get(7).unwrap(),
            Some(NonceRecord::Included {
                height: 12,
                tx_hash: hash.to_string()
            })
        );
        let pending: Vec<u64> = reloaded
            .submitted()
            .unwrap()
            .into_iter()
            .map(|(n, _)| n)
            .collect();
        assert_eq!(pending, vec![8]);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn included_nonce_cannot_regress_to_submitted() {
        let dir = tmp_dir();
        let store = InboxStore::new(&dir);
        let (hash, bytes) = submitted_record();

        store
            .record_submitted(2, 300, 1, 0, hash.into(), bytes.into())
            .unwrap();
        store.mark_included(2, 9).unwrap();
        assert!(store
            .record_submitted(2, 300, 1, 0, hash.into(), bytes.into())
            .is_err());
        assert_eq!(
            store.get(2).unwrap(),
            Some(NonceRecord::Included {
                height: 9,
                tx_hash: hash.to_string()
            })
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}
