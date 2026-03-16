//! MDBX batch write implementation.

use mptdb_common::error::{MptDbError, Result};
use mptdb_traits::{kv::Batch, types::WriteOptions};
use reth_libmdbx::{Environment, WriteFlags};

/// A buffered operation in the batch.
enum BatchOp {
    Set { key: Vec<u8>, value: Vec<u8> },
    Delete { key: Vec<u8> },
}

/// Accumulates set/delete operations and commits them in a single
/// MDBX write transaction.
pub struct MdbxBatch {
    env: Environment,
    ops: Vec<BatchOp>,
}

impl MdbxBatch {
    pub fn new(env: Environment) -> Self {
        Self { env, ops: Vec::with_capacity(1024) }
    }
}

impl Batch for MdbxBatch {
    fn set(&mut self, key: &[u8], value: &[u8]) -> Result<()> {
        self.ops.push(BatchOp::Set { key: key.to_vec(), value: value.to_vec() });
        Ok(())
    }

    fn delete(&mut self, key: &[u8]) -> Result<()> {
        self.ops.push(BatchOp::Delete { key: key.to_vec() });
        Ok(())
    }

    fn commit(&mut self, _opts: &WriteOptions) -> Result<()> {
        if self.ops.is_empty() {
            return Ok(());
        }

        let txn =
            self.env.begin_rw_txn().map_err(|e| MptDbError::Other(format!("batch rw txn: {e}")))?;
        let db = txn.open_db(None).map_err(|e| MptDbError::Other(format!("batch open db: {e}")))?;

        for op in &self.ops {
            match op {
                BatchOp::Set { key, value } => {
                    txn.put(db.dbi(), key, value, WriteFlags::empty())
                        .map_err(|e| MptDbError::Other(format!("batch put: {e}")))?;
                }
                BatchOp::Delete { key } => {
                    let _ = txn
                        .del(db.dbi(), key, None)
                        .map_err(|e| MptDbError::Other(format!("batch del: {e}")))?;
                }
            }
        }

        txn.commit().map_err(|e| MptDbError::Other(format!("batch commit: {e}")))?;

        self.ops.clear();
        Ok(())
    }

    fn len(&self) -> usize {
        self.ops.len()
    }

    fn is_empty(&self) -> bool {
        self.ops.is_empty()
    }

    fn reset(&mut self) {
        self.ops.clear();
    }

    fn close(&mut self) -> Result<()> {
        self.ops.clear();
        Ok(())
    }
}
