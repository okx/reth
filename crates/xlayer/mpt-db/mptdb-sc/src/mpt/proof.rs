use alloy_primitives::{keccak256, Address, Bytes, B256, U256};
use alloy_rlp::Decodable;
use alloy_trie::{Nibbles, EMPTY_ROOT_HASH};
use mptdb_common::error::{MptDbError, Result};
use reth_primitives_traits::Account;
use reth_trie_common::{AccountProof, StorageProof};

use super::{
    encoding::decode_node,
    node::{ChildRef, MptNode},
    persisted::PersistedTrieStore,
};

/// Build an Ethereum AccountProof from a committed state root by walking the
/// persisted trie store node-by-node (without loading the full tree into memory).
pub(crate) fn build_account_proof_from_root(
    store: &PersistedTrieStore,
    state_root: B256,
    address: Address,
    slots: &[B256],
) -> Result<AccountProof> {
    let account_path = Nibbles::unpack(keccak256(address));

    // Empty trie: non-existent account
    if state_root == EMPTY_ROOT_HASH {
        let storage_proofs = slots.iter().map(|slot| StorageProof::new(*slot)).collect();
        return Ok(AccountProof {
            address,
            info: None,
            proof: vec![],
            storage_root: EMPTY_ROOT_HASH,
            storage_proofs,
        });
    }

    // Collect account trie proof path
    let account_proof_nodes = collect_proof_path(store, state_root, &account_path)?;

    // Try to decode account from proof
    let (info, storage_root) = extract_account_from_proof(&account_proof_nodes, &account_path)?;

    // Build storage proofs
    let storage_proofs = if info.is_some() && storage_root != EMPTY_ROOT_HASH {
        slots
            .iter()
            .map(|slot| {
                let slot_path = Nibbles::unpack(keccak256(slot));
                let proof_nodes = collect_proof_path(store, storage_root, &slot_path)?;
                let value = extract_storage_value_from_proof(&proof_nodes, &slot_path)?;
                Ok(StorageProof { key: *slot, nibbles: slot_path, value, proof: proof_nodes })
            })
            .collect::<Result<Vec<_>>>()?
    } else {
        // Non-existent account or empty storage: empty proofs with value=0
        slots.iter().map(|slot| StorageProof::new(*slot)).collect()
    };

    Ok(AccountProof { address, info, proof: account_proof_nodes, storage_root, storage_proofs })
}

/// Walk from `root` following `path` nibbles, collecting RLP of each visited node.
fn collect_proof_path(
    store: &PersistedTrieStore,
    root: B256,
    path: &Nibbles,
) -> Result<Vec<Bytes>> {
    let mut proof = Vec::new();
    let mut current_hash = root;
    let mut offset = 0usize;

    loop {
        let rlp = store
            .get_node(current_hash)?
            .ok_or_else(|| MptDbError::Other(format!("proof: node not found: {current_hash}")))?;
        proof.push(Bytes::from(rlp.clone()));

        let node =
            decode_node(&rlp).map_err(|e| MptDbError::Other(format!("proof: decode node: {e}")))?;

        match node {
            MptNode::Leaf(_) => {
                // End of path (whether matching or not — proof is complete)
                break;
            }
            MptNode::Extension(ext) => {
                let ext_nibs = ext.nibbles.to_vec();
                let remaining_len = path.len() - offset;
                if remaining_len < ext_nibs.len() {
                    break;
                }
                let path_segment: Vec<u8> =
                    (0..ext_nibs.len()).map(|i| path.get_unchecked(offset + i)).collect();
                if path_segment != ext_nibs {
                    // Path diverges at extension — proof ends here
                    break;
                }
                offset += ext_nibs.len();
                match ext.child {
                    ChildRef::Hash(h) => {
                        current_hash = h;
                    }
                    ChildRef::Inline(inline_rlp) => {
                        // Inline child: add its RLP and continue parsing
                        proof.push(Bytes::from(inline_rlp.clone()));
                        // Cannot recurse further into inline — proof complete
                        break;
                    }
                    ChildRef::Arena(_) => {
                        return Err(MptDbError::Other(
                            "proof: unexpected Arena child in persisted node".to_string(),
                        ));
                    }
                }
            }
            MptNode::Branch(branch) => {
                if offset >= path.len() {
                    // Path consumed at branch — proof ends here
                    break;
                }
                let nibble = path.get_unchecked(offset) as usize;
                offset += 1;
                match &branch.children[nibble] {
                    None => {
                        // No child at this nibble — exclusion proof complete
                        break;
                    }
                    Some(ChildRef::Hash(h)) => {
                        current_hash = *h;
                    }
                    Some(ChildRef::Inline(inline_rlp)) => {
                        proof.push(Bytes::from(inline_rlp.clone()));
                        break;
                    }
                    Some(ChildRef::Arena(_)) => {
                        return Err(MptDbError::Other(
                            "proof: unexpected Arena child in persisted node".to_string(),
                        ));
                    }
                }
            }
        }
    }

    Ok(proof)
}

/// Extract account info and storage_root from an account proof.
/// Returns (None, EMPTY_ROOT_HASH) if account doesn't exist.
fn extract_account_from_proof(
    proof: &[Bytes],
    account_path: &Nibbles,
) -> Result<(Option<Account>, B256)> {
    if proof.is_empty() {
        return Ok((None, EMPTY_ROOT_HASH));
    }

    let last = &proof[proof.len() - 1];
    let node = decode_node(last)
        .map_err(|e| MptDbError::Other(format!("proof: decode last node: {e}")))?;

    match node {
        MptNode::Leaf(leaf) => {
            // Check if the leaf's path suffix matches the end of account_path
            if account_path.ends_with(&leaf.nibbles) {
                let trie_account = decode_account_leaf(&leaf.value)?;
                let info = Account {
                    nonce: trie_account.nonce,
                    balance: trie_account.balance,
                    bytecode_hash: if trie_account.code_hash ==
                        alloy_primitives::B256::from(alloy_trie::KECCAK_EMPTY)
                    {
                        None
                    } else {
                        Some(trie_account.code_hash)
                    },
                };
                Ok((Some(info), trie_account.storage_root))
            } else {
                // Leaf doesn't match — account doesn't exist (exclusion proof)
                Ok((None, EMPTY_ROOT_HASH))
            }
        }
        _ => {
            // Branch or Extension at end — account doesn't exist at this path
            Ok((None, EMPTY_ROOT_HASH))
        }
    }
}

/// Decode a TrieAccount from RLP leaf value bytes.
fn decode_account_leaf(leaf_value: &[u8]) -> Result<alloy_trie::TrieAccount> {
    alloy_trie::TrieAccount::decode(&mut &leaf_value[..])
        .map_err(|e| MptDbError::Other(format!("decode account leaf RLP: {e}")))
}

/// Extract a storage value from a storage proof.
fn extract_storage_value_from_proof(proof: &[Bytes], slot_path: &Nibbles) -> Result<U256> {
    if proof.is_empty() {
        return Ok(U256::ZERO);
    }

    let last = &proof[proof.len() - 1];
    let node = decode_node(last)
        .map_err(|e| MptDbError::Other(format!("proof: decode storage node: {e}")))?;

    match node {
        MptNode::Leaf(leaf) => {
            if slot_path.ends_with(&leaf.nibbles) {
                // Decode the storage value from RLP
                let value = U256::decode(&mut &leaf.value[..])
                    .map_err(|e| MptDbError::Other(format!("decode storage value RLP: {e}")))?;
                Ok(value)
            } else {
                Ok(U256::ZERO)
            }
        }
        _ => Ok(U256::ZERO),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_rlp::Encodable;
    use alloy_trie::KECCAK_EMPTY;
    use tempfile::TempDir;

    use crate::mpt::tree::MptTree;

    fn tmp_store() -> (PersistedTrieStore, TempDir) {
        let dir = TempDir::new().unwrap();
        let store = PersistedTrieStore::open(dir.path()).unwrap();
        (store, dir)
    }

    /// Build and persist a simple account trie with one account.
    fn build_single_account(
        store: &PersistedTrieStore,
        address: Address,
        nonce: u64,
        balance: U256,
        storage_root: B256,
        code_hash: B256,
    ) -> B256 {
        let mut tree = MptTree::new();
        let hashed = keccak256(address);
        let key = Nibbles::unpack(hashed);
        let trie_account = alloy_trie::TrieAccount { nonce, balance, storage_root, code_hash };
        let mut rlp_buf = Vec::new();
        trie_account.encode(&mut rlp_buf);
        tree.insert(&key, rlp_buf);
        let root = tree.root_hash();
        let blobs = tree.collect_node_blobs();
        store.persist_batch(&blobs, true).unwrap();
        root
    }

    /// Build and persist a storage trie with slots.
    fn build_storage_trie(store: &PersistedTrieStore, slots: &[(B256, U256)]) -> B256 {
        let mut tree = MptTree::new();
        for (slot, value) in slots {
            let key = Nibbles::unpack(keccak256(slot));
            let mut buf = Vec::new();
            value.encode(&mut buf);
            tree.insert(&key, buf);
        }
        let root = tree.root_hash();
        let blobs = tree.collect_node_blobs();
        store.persist_batch(&blobs, true).unwrap();
        root
    }

    /// T3.1: empty state_root -> non-existent account proof verify succeeds
    #[test]
    fn t3_1_empty_state_root() {
        let (store, _dir) = tmp_store();
        let addr = Address::repeat_byte(0x01);
        let proof = build_account_proof_from_root(&store, EMPTY_ROOT_HASH, addr, &[]).unwrap();
        assert!(proof.info.is_none());
        assert_eq!(proof.storage_root, EMPTY_ROOT_HASH);
        assert!(proof.proof.is_empty());
        proof.verify(EMPTY_ROOT_HASH).unwrap();
    }

    /// T3.2: existing EOA account (no storage) -> account proof verify succeeds
    #[test]
    fn t3_2_existing_eoa() {
        let (store, _dir) = tmp_store();
        let addr = Address::repeat_byte(0x02);
        let root =
            build_single_account(&store, addr, 5, U256::from(1000), EMPTY_ROOT_HASH, KECCAK_EMPTY);
        let proof = build_account_proof_from_root(&store, root, addr, &[]).unwrap();
        assert!(proof.info.is_some());
        let info = proof.info.as_ref().unwrap();
        assert_eq!(info.nonce, 5);
        assert_eq!(info.balance, U256::from(1000));
        proof.verify(root).unwrap();
    }

    /// T3.3: existing contract + existing slot -> account/storage proof verify succeeds
    #[test]
    fn t3_3_contract_with_storage() {
        let (store, _dir) = tmp_store();
        let addr = Address::repeat_byte(0x03);
        let slot = B256::repeat_byte(0x01);
        let slot_val = U256::from(42);
        let storage_root = build_storage_trie(&store, &[(slot, slot_val)]);
        let code_hash = B256::repeat_byte(0xcc);
        let root = build_single_account(&store, addr, 1, U256::from(500), storage_root, code_hash);

        let proof = build_account_proof_from_root(&store, root, addr, &[slot]).unwrap();
        assert!(proof.info.is_some());
        assert_eq!(proof.storage_root, storage_root);
        assert_eq!(proof.storage_proofs.len(), 1);
        assert_eq!(proof.storage_proofs[0].value, slot_val);
        proof.verify(root).unwrap();
    }

    /// T3.4: existing contract + missing slot -> storage exclusion proof verify succeeds
    #[test]
    fn t3_4_missing_slot_exclusion() {
        let (store, _dir) = tmp_store();
        let addr = Address::repeat_byte(0x04);
        let existing_slot = B256::repeat_byte(0x01);
        let missing_slot = B256::repeat_byte(0x02);
        let storage_root = build_storage_trie(&store, &[(existing_slot, U256::from(99))]);
        let code_hash = B256::repeat_byte(0xdd);
        let root = build_single_account(&store, addr, 1, U256::from(100), storage_root, code_hash);

        let proof = build_account_proof_from_root(&store, root, addr, &[missing_slot]).unwrap();
        assert_eq!(proof.storage_proofs[0].value, U256::ZERO);
        proof.verify(root).unwrap();
    }

    /// T3.5: missing account + requested slots -> proof verify succeeds, all slot value=0
    #[test]
    fn t3_5_missing_account_with_slots() {
        let (store, _dir) = tmp_store();
        let existing_addr = Address::repeat_byte(0x05);
        let root = build_single_account(
            &store,
            existing_addr,
            1,
            U256::from(100),
            EMPTY_ROOT_HASH,
            KECCAK_EMPTY,
        );

        let missing_addr = Address::repeat_byte(0x06);
        let slot = B256::repeat_byte(0x01);
        let proof = build_account_proof_from_root(&store, root, missing_addr, &[slot]).unwrap();
        assert!(proof.info.is_none());
        assert_eq!(proof.storage_proofs[0].value, U256::ZERO);
        proof.verify(root).unwrap();
    }

    /// T3.6: multi slot request -> storage_proofs order matches input slots order
    #[test]
    fn t3_6_multi_slot_order() {
        let (store, _dir) = tmp_store();
        let addr = Address::repeat_byte(0x07);
        let slot1 = B256::repeat_byte(0x01);
        let slot2 = B256::repeat_byte(0x02);
        let slot3 = B256::repeat_byte(0x03);
        let storage_root = build_storage_trie(
            &store,
            &[(slot1, U256::from(10)), (slot2, U256::from(20)), (slot3, U256::from(30))],
        );
        let code_hash = B256::repeat_byte(0xee);
        let root = build_single_account(&store, addr, 1, U256::from(100), storage_root, code_hash);

        let slots = &[slot3, slot1, slot2];
        let proof = build_account_proof_from_root(&store, root, addr, slots).unwrap();
        assert_eq!(proof.storage_proofs.len(), 3);
        assert_eq!(proof.storage_proofs[0].key, slot3);
        assert_eq!(proof.storage_proofs[1].key, slot1);
        assert_eq!(proof.storage_proofs[2].key, slot2);
        proof.verify(root).unwrap();
    }

    /// T3.7: corrupt account leaf RLP -> Err
    #[test]
    fn t3_7_corrupt_account_leaf() {
        let result = decode_account_leaf(&[0xff, 0xfe, 0xfd]);
        assert!(result.is_err());
    }

    /// T3.8: proof builder does not modify persisted store / manifest
    #[test]
    fn t3_8_proof_no_side_effects() {
        let (store, _dir) = tmp_store();
        let addr = Address::repeat_byte(0x08);
        let root =
            build_single_account(&store, addr, 1, U256::from(100), EMPTY_ROOT_HASH, KECCAK_EMPTY);

        // Count nodes before proof
        let count_before = {
            let mut iter = store.iter_all_nodes().unwrap();
            let mut count = 0u64;
            if iter.first() {
                count += 1;
                while iter.next() {
                    count += 1;
                }
            }
            iter.close().unwrap();
            count
        };

        build_account_proof_from_root(&store, root, addr, &[]).unwrap();

        // Count nodes after proof
        let count_after = {
            let mut iter = store.iter_all_nodes().unwrap();
            let mut count = 0u64;
            if iter.first() {
                count += 1;
                while iter.next() {
                    count += 1;
                }
            }
            iter.close().unwrap();
            count
        };

        assert_eq!(count_before, count_after);
    }
}
