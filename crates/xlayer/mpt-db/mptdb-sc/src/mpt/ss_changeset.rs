//! Conversion from revm `BundleState` to mptdb-ss `ChangeSet`.
//!
//! This is the write-path adapter for reth integration: after a block is
//! executed, the `BundleState` is converted into an SS changeset that is
//! written to `EVMStateStore` (the flat-KV state store).
//!
//! Only state data is written:
//! - Account fields (nonce, balance, code_hash) via `EvmKeyKind::Account`.
//! - Storage slots via `EvmKeyKind::Storage`.
//! Bytecode bodies are NOT written here; they remain in reth's MDBX.

use mptdb_common::evm_keys::{make_account_key, make_storage_key};
use mptdb_proto::{ChangeSet, KvPair};
use mptdb_ss::evm::store::EVMStateStore;
use revm_database::BundleState;

/// Build an SS [`ChangeSet`] from a revm [`BundleState`].
///
/// Rules:
/// - Account deleted (`present_info == None`) → delete Account key.
/// - Account created/modified (`present_info == Some(info)`) → write Account key with encoded
///   `(nonce, balance, code_hash)`.
/// - Storage slot `present_value == 0` → delete Storage key (slot cleared).
/// - Storage slot `present_value != 0` → write Storage key with 32-byte big-endian value.
/// - Bytecode changes are ignored (not stored in SS).
///
/// Both address and slot keys are **raw** (not keccak-hashed); the SS uses raw
/// addresses while the SC (MPT) uses keccak addresses.  reth's `BundleState`
/// carries raw addresses, so no hashing is needed here.
pub fn bundle_to_ss_changeset(bundle: &BundleState) -> ChangeSet {
    let mut pairs: Vec<KvPair> = Vec::new();

    for (address, bundle_account) in &bundle.state {
        let addr_bytes: [u8; 20] = address.into_array();

        // ── Account change ─────────────────────────────────────────────────
        let account_key = make_account_key(&addr_bytes);

        match &bundle_account.info {
            None => {
                // Account destroyed (selfdestruct or pruned) → tombstone.
                pairs.push(KvPair { delete: true, key: account_key.to_vec(), value: vec![] });
            }
            Some(info) => {
                let balance_bytes: [u8; 32] = info.balance.to_be_bytes();
                let code_hash_bytes: [u8; 32] = *info.code_hash;
                let value =
                    EVMStateStore::encode_account_value(info.nonce, balance_bytes, code_hash_bytes);
                pairs.push(KvPair {
                    delete: false,
                    key: account_key.to_vec(),
                    value: value.to_vec(),
                });
            }
        }

        // ── Storage changes ────────────────────────────────────────────────
        for (slot, storage_slot) in &bundle_account.storage {
            let slot_bytes: [u8; 32] = slot.to_be_bytes();
            let storage_key = make_storage_key(&addr_bytes, &slot_bytes);

            if storage_slot.present_value.is_zero() {
                // Slot cleared → tombstone.
                pairs.push(KvPair { delete: true, key: storage_key.to_vec(), value: vec![] });
            } else {
                let value_bytes: [u8; 32] = storage_slot.present_value.to_be_bytes();
                pairs.push(KvPair {
                    delete: false,
                    key: storage_key.to_vec(),
                    value: value_bytes.to_vec(),
                });
            }
        }

        // Bytecode changes: ignored — not stored in SS.
    }

    ChangeSet { pairs }
}
