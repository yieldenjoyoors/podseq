//! Persistent storage for podseq chain state and block data.
//!
//! Layout: `blocks/{height}` (BCS), `state.json` (chain state), and
//! `pending.json` (unsettled heights). State-bearing files are written
//! atomically (temp file + rename) for crash safety.

#![forbid(unsafe_code)]

use std::path::Path;

use thiserror::Error;

mod block_store;
mod pending_store;
mod state_store;

pub use block_store::BlockStore;
pub use pending_store::PendingStore;
pub use state_store::{ChainState, StateStore};

/// Errors returned by the storage layer.
#[derive(Debug, Error)]
pub enum StoreError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("serde error: {0}")]
    Serde(#[from] serde_json::Error),
    #[error("block not found: height {0}")]
    BlockNotFound(u64),
    #[error("state not found")]
    StateNotFound,
}

/// Creates the storage directory layout under `data_dir`.
pub fn init(data_dir: &Path) -> Result<(), StoreError> {
    std::fs::create_dir_all(data_dir.join("blocks"))?;
    Ok(())
}
