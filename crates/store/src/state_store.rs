//! Persists chain state (fork-choice points and sync metadata) as JSON.
//! Writes are atomic (temp file + rename) for crash safety.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::StoreError;

/// Persistent chain state: fork-choice hashes plus sync metadata.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ChainState {
    /// Hex-encoded hash.
    pub head: String,
    /// Hex-encoded hash.
    pub safe: String,
    /// Hex-encoded hash.
    pub finalized: String,
    /// Produced by sequencer, or synced by full node.
    pub height: u64,
    /// Last timestamp used for block production.
    pub timestamp: u64,
}

/// Filesystem-backed store for chain state.
pub struct StateStore {
    path: PathBuf,
}

impl StateStore {
    /// Creates a state store at `data_dir/state.json`.
    pub fn new(data_dir: &Path) -> Self {
        Self {
            path: data_dir.join("state.json"),
        }
    }

    /// Loads chain state; returns `None` if no state file exists yet.
    pub fn load(&self) -> Result<Option<ChainState>, StoreError> {
        if !self.path.exists() {
            return Ok(None);
        }
        let text = std::fs::read_to_string(&self.path)?;
        let state: ChainState = serde_json::from_str(&text)?;
        Ok(Some(state))
    }

    /// Persists chain state atomically.
    pub fn save(&self, state: &ChainState) -> Result<(), StoreError> {
        let tmp = self.path.with_extension("json.tmp");
        let text = serde_json::to_string_pretty(state)?;
        std::fs::write(&tmp, &text)?;
        std::fs::rename(&tmp, &self.path)?;
        tracing::debug!(height = state.height, "chain state saved");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "podseq-state-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn save_and_load() {
        let dir = tmp_dir();
        let store = StateStore::new(&dir);
        let state = ChainState {
            head: "0xabc".into(),
            safe: "0xabc".into(),
            finalized: "0xabc".into(),
            height: 42,
            timestamp: 1700000000,
        };
        store.save(&state).unwrap();
        let loaded = store.load().unwrap().unwrap();
        assert_eq!(loaded.height, 42);
        assert_eq!(loaded.head, "0xabc");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn load_missing_returns_none() {
        let dir = tmp_dir();
        let store = StateStore::new(&dir);
        assert!(store.load().unwrap().is_none());
        std::fs::remove_dir_all(&dir).ok();
    }
}
