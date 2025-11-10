//! Chain information file for datadir.
//!
//! This file stores minimal chain metadata to enable loading chain spec from database
//! without requiring the chain parameter.

use alloy_primitives::B256;
use serde::{Deserialize, Serialize};
use std::path::Path;

/// Chain information stored in datadir.
///
/// This is used to break the circular dependency between needing the chain to determine
/// the datadir path and needing the datadir path to load the chain from the database.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChainInfo {
    /// Chain ID
    pub chain_id: u64,

    /// Genesis block hash
    pub genesis_hash: B256,

    /// Genesis block number
    pub genesis_block_number: u64,

    /// Creation timestamp (RFC3339 format)
    pub created_at: String,
}

impl ChainInfo {
    /// File name for chain info
    pub const FILE_NAME: &'static str = ".chain-info";

    /// Read chain info from datadir
    pub fn read(datadir: &Path) -> eyre::Result<Self> {
        let path = datadir.join(Self::FILE_NAME);
        let content = std::fs::read_to_string(&path).map_err(|e| {
            eyre::eyre!(
                "Failed to read chain info from {}: {}. \
                 Please specify --chain parameter or run init first.",
                path.display(),
                e
            )
        })?;
        let chain_info: Self = serde_json::from_str(&content).map_err(|e| {
            eyre::eyre!("Failed to parse chain info from {}: {}", path.display(), e)
        })?;
        Ok(chain_info)
    }

    /// Write chain info to datadir
    pub fn write(&self, datadir: &Path) -> eyre::Result<()> {
        let path = datadir.join(Self::FILE_NAME);
        let content = serde_json::to_string_pretty(self)?;
        std::fs::write(&path, content)
            .map_err(|e| eyre::eyre!("Failed to write chain info to {}: {}", path.display(), e))?;
        Ok(())
    }

    /// Check if chain info exists in datadir
    pub fn exists(datadir: &Path) -> bool {
        datadir.join(Self::FILE_NAME).exists()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::b256;

    #[test]
    fn test_chain_info_read_write() {
        let temp_dir = tempfile::tempdir().unwrap();
        let chain_info = ChainInfo {
            chain_id: 196,
            genesis_hash: b256!("dc33d8c0ec9de14fc2c21bd6077309a0a856df22821bd092a2513426e096a789"),
            genesis_block_number: 42810021,
            created_at: "2025-11-10T10:00:00Z".to_string(),
        };

        // Write
        chain_info.write(temp_dir.path()).unwrap();

        // Check exists
        assert!(ChainInfo::exists(temp_dir.path()));

        // Read
        let loaded = ChainInfo::read(temp_dir.path()).unwrap();
        assert_eq!(loaded.chain_id, chain_info.chain_id);
        assert_eq!(loaded.genesis_hash, chain_info.genesis_hash);
        assert_eq!(loaded.genesis_block_number, chain_info.genesis_block_number);
    }
}

