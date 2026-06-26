//! Filesystem-backed block storage keyed by height.
//!
//! Each block is stored as a BCS-encoded file at `blocks/{height}`.

use std::path::{Path, PathBuf};

use podseq_core::Block;

use crate::StoreError;

/// Filesystem-backed store for blocks indexed by height.
pub struct BlockStore {
    dir: PathBuf,
}

impl BlockStore {
    /// Creates a block store rooted at `data_dir/blocks`.
    pub fn new(data_dir: &Path) -> Self {
        let dir = data_dir.join("blocks");
        std::fs::create_dir_all(&dir).ok();
        Self { dir }
    }

    /// Persists a block at its height, overwriting any existing entry.
    pub fn put(&self, block: &Block) -> Result<(), StoreError> {
        let path = self.dir.join(format!("{}", block.header.height));
        let bytes = bcs::to_bytes(block)
            .map_err(|e| StoreError::Serde(serde::de::Error::custom(e.to_string())))?;
        std::fs::write(&path, &bytes)?;
        tracing::debug!(height = block.header.height, path = %path.display(), "block stored");
        Ok(())
    }

    /// Retrieves a block by height.
    pub fn get(&self, height: u64) -> Result<Block, StoreError> {
        let path = self.dir.join(format!("{height}"));
        if !path.exists() {
            return Err(StoreError::BlockNotFound(height));
        }
        let bytes = std::fs::read(&path)?;
        let block: Block = bcs::from_bytes(&bytes)
            .map_err(|e| StoreError::Serde(serde::de::Error::custom(e.to_string())))?;
        Ok(block)
    }

    /// Returns whether a block at the given height is stored.
    pub fn has(&self, height: u64) -> bool {
        self.dir.join(format!("{height}")).exists()
    }

    /// Returns the highest stored block height, or `None` if empty.
    pub fn latest_height(&self) -> Option<u64> {
        std::fs::read_dir(&self.dir)
            .ok()?
            .filter_map(|e| e.ok())
            .filter_map(|e| e.file_name().to_string_lossy().parse::<u64>().ok())
            .max()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use podseq_core::Header;

    fn tmp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "podseq-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn sample_block(height: u64) -> Block {
        Block {
            header: Header {
                height,
                parent_hash: [0; 32],
                state_root: [height as u8; 32],
                timestamp: height,
            },
            data: vec![1, 2, 3],
            signature: None,
        }
    }

    #[test]
    fn put_and_get() {
        let dir = tmp_dir();
        let store = BlockStore::new(&dir);
        let block = sample_block(42);
        store.put(&block).unwrap();
        let got = store.get(42).unwrap();
        assert_eq!(got.header.height, 42);
        assert_eq!(got.data, block.data);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn get_missing_fails() {
        let dir = tmp_dir();
        let store = BlockStore::new(&dir);
        assert!(store.get(999).is_err());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn latest_height() {
        let dir = tmp_dir();
        let store = BlockStore::new(&dir);
        store.put(&sample_block(1)).unwrap();
        store.put(&sample_block(5)).unwrap();
        store.put(&sample_block(3)).unwrap();
        assert_eq!(store.latest_height(), Some(5));
        std::fs::remove_dir_all(&dir).ok();
    }
}
