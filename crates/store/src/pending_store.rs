//! Tracks produced-but-unsettled block heights for crash recovery.
//!
//! The sequencer marks a height pending when it produces a block and clears it
//! once DA + settlement finalize it. On restart, any lingering heights are
//! re-submitted to the finalizer.

#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::StoreError;

/// Invariant: heights are sorted and unique.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct PendingFile {
    heights: Vec<u64>,
}

/// Records block heights that are pending finalization (crash recovery).
pub struct PendingStore {
    path: PathBuf,
}

impl PendingStore {
    /// Creates a pending store at `data_dir/pending.json`.
    pub fn new(data_dir: &Path) -> Self {
        Self {
            path: data_dir.join("pending.json"),
        }
    }

    fn load_raw(&self) -> Result<PendingFile, StoreError> {
        if !self.path.exists() {
            return Ok(PendingFile::default());
        }
        let text = std::fs::read_to_string(&self.path)?;
        let mut file: PendingFile = if text.trim().is_empty() {
            PendingFile::default()
        } else {
            serde_json::from_str(&text)?
        };
        file.heights.sort_unstable();
        file.heights.dedup();
        Ok(file)
    }

    fn save_raw(&self, file: &PendingFile) -> Result<(), StoreError> {
        let tmp = self.path.with_extension("json.tmp");
        let text = serde_json::to_string_pretty(file)?;
        std::fs::write(&tmp, &text)?;
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }

    fn mutate<F>(&self, f: F) -> Result<(), StoreError>
    where
        F: FnOnce(&mut Vec<u64>),
    {
        let raw = self.load_raw()?;
        let mut heights = raw.heights;
        f(&mut heights);
        self.save_raw(&PendingFile { heights })
    }

    /// Marks `height` as produced-but-unsettled (idempotent).
    pub fn mark(&self, height: u64) -> Result<(), StoreError> {
        self.mutate(|heights| {
            if let Err(pos) = heights.binary_search(&height) {
                heights.insert(pos, height);
            }
        })
    }

    /// Clears `height` after it is finalized (idempotent).
    pub fn clear(&self, height: u64) -> Result<(), StoreError> {
        self.mutate(|heights| {
            if let Ok(pos) = heights.binary_search(&height) {
                heights.remove(pos);
            }
        })
    }

    /// Returns all pending heights in ascending order.
    pub fn pending(&self) -> Result<Vec<u64>, StoreError> {
        Ok(self.load_raw()?.heights)
    }

    /// Returns whether there are no pending heights.
    pub fn is_empty(&self) -> Result<bool, StoreError> {
        Ok(self.load_raw()?.heights.is_empty())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "podseq-pending-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn empty_when_no_file() {
        let dir = tmp_dir();
        let store = PendingStore::new(&dir);
        assert!(store.pending().unwrap().is_empty());
        assert!(store.is_empty().unwrap());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn mark_then_pending_lists_it() {
        let dir = tmp_dir();
        let store = PendingStore::new(&dir);
        store.mark(3).unwrap();
        assert_eq!(store.pending().unwrap(), vec![3]);
        assert!(!store.is_empty().unwrap());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn mark_is_idempotent() {
        let dir = tmp_dir();
        let store = PendingStore::new(&dir);
        store.mark(5).unwrap();
        store.mark(5).unwrap();
        assert_eq!(store.pending().unwrap(), vec![5]);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn pending_is_sorted_ascending() {
        let dir = tmp_dir();
        let store = PendingStore::new(&dir);
        store.mark(7).unwrap();
        store.mark(2).unwrap();
        store.mark(5).unwrap();
        assert_eq!(store.pending().unwrap(), vec![2, 5, 7]);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn clear_removes_a_height() {
        let dir = tmp_dir();
        let store = PendingStore::new(&dir);
        store.mark(1).unwrap();
        store.mark(2).unwrap();
        store.mark(3).unwrap();
        store.clear(2).unwrap();
        assert_eq!(store.pending().unwrap(), vec![1, 3]);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn clear_missing_is_idempotent() {
        let dir = tmp_dir();
        let store = PendingStore::new(&dir);
        store.clear(99).unwrap();
        assert!(store.is_empty().unwrap());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn survives_reload_from_disk() {
        let dir = tmp_dir();
        {
            let store = PendingStore::new(&dir);
            store.mark(10).unwrap();
            store.mark(20).unwrap();
        }
        let store = PendingStore::new(&dir);
        assert_eq!(store.pending().unwrap(), vec![10, 20]);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn clearing_all_leaves_empty_file() {
        let dir = tmp_dir();
        let store = PendingStore::new(&dir);
        store.mark(4).unwrap();
        store.clear(4).unwrap();
        assert!(store.is_empty().unwrap());
        assert_eq!(store.pending().unwrap(), Vec::<u64>::new());
        std::fs::remove_dir_all(&dir).ok();
    }
}
