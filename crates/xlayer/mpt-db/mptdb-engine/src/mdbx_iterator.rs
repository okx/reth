//! MDBX iterator implementing `KvIterator`.
//!
//! Owns a read-only transaction and cursor, making it `'static` compatible
//! for use as `Box<dyn KvIterator>`.

use mptdb_common::error::{MptDbError, Result};
use mptdb_traits::{kv::KvIterator, types::IterOptions};
use reth_libmdbx::{Environment, Transaction, RO};
use std::borrow::Cow;

/// MDBX-backed iterator that owns its read transaction and cursor.
///
/// The cursor is lazily created from the transaction. Key/value pairs
/// are cached after each positioning operation.
pub struct MdbxIterator {
    /// Read-only transaction (keeps data stable for the iterator's lifetime).
    txn: Transaction<RO>,
    /// Raw DBI handle for the default database.
    dbi: reth_libmdbx::ffi::MDBX_dbi,
    /// Cached current key (populated after each seek/next/prev).
    current_key: Vec<u8>,
    /// Cached current value.
    current_value: Vec<u8>,
    /// Whether the iterator is positioned at a valid entry.
    is_valid: bool,
    /// Cursor pointer (raw, because we need to manage lifetime manually).
    /// Created lazily on first positioning call.
    cursor: Option<*mut reth_libmdbx::ffi::MDBX_cursor>,
    /// Upper bound for range iteration (exclusive).
    upper_bound: Option<Vec<u8>>,
    /// Lower bound for range iteration (inclusive).
    lower_bound: Option<Vec<u8>>,
    /// Error from the last operation, if any.
    last_error: Option<MptDbError>,
}

// SAFETY: MdbxIterator owns its txn (which is Send+Sync) and cursor.
// The cursor is tied to the txn and only accessed from one thread.
unsafe impl Send for MdbxIterator {}

impl MdbxIterator {
    /// Create a new iterator from an MDBX environment.
    pub fn new(env: &Environment, opts: &IterOptions) -> Result<Box<dyn KvIterator>> {
        let txn = env.begin_ro_txn().map_err(|e| MptDbError::Other(format!("iter ro txn: {e}")))?;
        let db = txn.open_db(None).map_err(|e| MptDbError::Other(format!("iter open db: {e}")))?;
        let dbi = db.dbi();

        Ok(Box::new(Self {
            txn,
            dbi,
            current_key: Vec::new(),
            current_value: Vec::new(),
            is_valid: false,
            cursor: None,
            upper_bound: opts.upper_bound.clone(),
            lower_bound: opts.lower_bound.clone(),
            last_error: None,
        }))
    }

    /// Ensure cursor is created.
    fn ensure_cursor(&mut self) -> bool {
        if self.cursor.is_some() {
            return true;
        }
        match self.txn.cursor(self.dbi) {
            Ok(cursor) => {
                // Extract raw pointer from cursor before it's dropped.
                // We manage the cursor lifetime ourselves.
                let raw = cursor_to_raw(cursor);
                self.cursor = Some(raw);
                true
            }
            Err(e) => {
                self.last_error = Some(MptDbError::Other(format!("create cursor: {e}")));
                false
            }
        }
    }

    /// Position cursor using a raw MDBX operation.
    fn cursor_op(&mut self, op: u32) -> bool {
        if !self.ensure_cursor() {
            return false;
        }
        let cursor_ptr = self.cursor.unwrap();

        unsafe {
            let mut key_val =
                reth_libmdbx::ffi::MDBX_val { iov_base: std::ptr::null_mut(), iov_len: 0 };
            let mut data_val =
                reth_libmdbx::ffi::MDBX_val { iov_base: std::ptr::null_mut(), iov_len: 0 };

            let rc =
                reth_libmdbx::ffi::mdbx_cursor_get(cursor_ptr, &mut key_val, &mut data_val, op);

            if rc == 0 {
                let key_slice =
                    std::slice::from_raw_parts(key_val.iov_base as *const u8, key_val.iov_len);
                let val_slice =
                    std::slice::from_raw_parts(data_val.iov_base as *const u8, data_val.iov_len);

                if let Some(ref ub) = self.upper_bound {
                    if key_slice >= ub.as_slice() {
                        self.is_valid = false;
                        return false;
                    }
                }
                if let Some(ref lb) = self.lower_bound {
                    if key_slice < lb.as_slice() {
                        self.is_valid = false;
                        return false;
                    }
                }

                self.current_key.clear();
                self.current_key.extend_from_slice(key_slice);
                self.current_value.clear();
                self.current_value.extend_from_slice(val_slice);
                self.is_valid = true;
                true
            } else {
                self.is_valid = false;
                false
            }
        }
    }

    /// Seek to a key using MDBX_SET_RANGE (>= key).
    fn cursor_seek(&mut self, key: &[u8]) -> bool {
        if !self.ensure_cursor() {
            return false;
        }
        let cursor_ptr = self.cursor.unwrap();

        unsafe {
            let mut key_val = reth_libmdbx::ffi::MDBX_val {
                iov_base: key.as_ptr() as *mut _,
                iov_len: key.len(),
            };
            let mut data_val =
                reth_libmdbx::ffi::MDBX_val { iov_base: std::ptr::null_mut(), iov_len: 0 };

            let rc = reth_libmdbx::ffi::mdbx_cursor_get(
                cursor_ptr,
                &mut key_val,
                &mut data_val,
                reth_libmdbx::ffi::MDBX_SET_RANGE,
            );

            if rc == 0 {
                let key_slice =
                    std::slice::from_raw_parts(key_val.iov_base as *const u8, key_val.iov_len);
                let val_slice =
                    std::slice::from_raw_parts(data_val.iov_base as *const u8, data_val.iov_len);

                if let Some(ref ub) = self.upper_bound {
                    if key_slice >= ub.as_slice() {
                        self.is_valid = false;
                        return false;
                    }
                }

                self.current_key.clear();
                self.current_key.extend_from_slice(key_slice);
                self.current_value.clear();
                self.current_value.extend_from_slice(val_slice);
                self.is_valid = true;
                true
            } else {
                self.is_valid = false;
                false
            }
        }
    }
}

impl KvIterator for MdbxIterator {
    fn first(&mut self) -> bool {
        self.cursor_op(reth_libmdbx::ffi::MDBX_FIRST)
    }

    fn last(&mut self) -> bool {
        self.cursor_op(reth_libmdbx::ffi::MDBX_LAST)
    }

    fn valid(&self) -> bool {
        self.is_valid
    }

    fn seek_ge(&mut self, key: &[u8]) -> bool {
        self.cursor_seek(key)
    }

    fn seek_lt(&mut self, key: &[u8]) -> bool {
        // Seek to >= key, then go back one.
        if self.cursor_seek(key) {
            // Check if we landed exactly on the key.
            if self.current_key.as_slice() == key {
                // Go back one.
                return self.cursor_op(reth_libmdbx::ffi::MDBX_PREV);
            }
            // We're past the key, go back one.
            return self.cursor_op(reth_libmdbx::ffi::MDBX_PREV);
        }
        // Key is past the end — position at last entry.
        self.cursor_op(reth_libmdbx::ffi::MDBX_LAST)
    }

    fn next(&mut self) -> bool {
        self.cursor_op(reth_libmdbx::ffi::MDBX_NEXT)
    }

    fn next_prefix(&mut self) -> bool {
        // MDBX doesn't have native prefix skip; just advance one.
        self.next()
    }

    fn prev(&mut self) -> bool {
        self.cursor_op(reth_libmdbx::ffi::MDBX_PREV)
    }

    fn key(&self) -> &[u8] {
        &self.current_key
    }

    fn value(&self) -> &[u8] {
        &self.current_value
    }

    fn error(&self) -> Option<&MptDbError> {
        self.last_error.as_ref()
    }

    fn close(&mut self) -> Result<()> {
        if let Some(ptr) = self.cursor.take() {
            unsafe {
                reth_libmdbx::ffi::mdbx_cursor_close(ptr);
            }
        }
        self.is_valid = false;
        Ok(())
    }
}

impl Drop for MdbxIterator {
    fn drop(&mut self) {
        if let Some(ptr) = self.cursor.take() {
            unsafe {
                reth_libmdbx::ffi::mdbx_cursor_close(ptr);
            }
        }
    }
}

/// Extract raw cursor pointer from a reth_libmdbx::Cursor.
///
/// This is necessary because Cursor's lifetime is tied to Transaction,
/// but we need the cursor to be owned by MdbxIterator alongside the txn.
fn cursor_to_raw<K: reth_libmdbx::TransactionKind>(
    cursor: reth_libmdbx::Cursor<K>,
) -> *mut reth_libmdbx::ffi::MDBX_cursor {
    // The Cursor struct has a `cursor: *mut MDBX_cursor` field.
    // We use transmute to extract it, then forget the Cursor to prevent double-free.
    let ptr = unsafe {
        // Cursor layout: { txn: Transaction<K>, cursor: *mut MDBX_cursor }
        // We need the second field.
        let raw = std::ptr::read(
            (&cursor as *const reth_libmdbx::Cursor<K> as *const u8)
                .add(std::mem::size_of::<reth_libmdbx::Transaction<K>>())
                as *const *mut reth_libmdbx::ffi::MDBX_cursor,
        );
        std::mem::forget(cursor);
        raw
    };
    ptr
}
