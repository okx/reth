use alloy_primitives::B256;
use alloy_rlp::{Decodable, Encodable, Header};
use alloy_trie::Nibbles;

use super::{
    hash::{hash_rlp, should_inline},
    node::{BranchNode, ChildRef, ExtensionNode, LeafNode, MptNode},
};

/// Encode nibbles as hex-prefix (compact encoding).
///
/// Rules:
/// - flag = 0x20 if leaf, 0x00 if extension
/// - odd nibbles: first byte = flag | 0x10 | nibbles[0], rest packed
/// - even nibbles: first byte = flag, nibbles packed
pub fn encode_path(nibbles: &Nibbles, is_leaf: bool) -> Vec<u8> {
    let flag = if is_leaf { 0x20u8 } else { 0x00u8 };
    let nib_vec = nibbles.to_vec();

    if nib_vec.len() % 2 == 1 {
        // Odd: first byte encodes flag + first nibble
        let mut out = Vec::with_capacity(1 + nib_vec.len() / 2);
        out.push(flag | 0x10 | nib_vec[0]);
        for chunk in nib_vec[1..].chunks(2) {
            out.push((chunk[0] << 4) | chunk[1]);
        }
        out
    } else {
        // Even: first byte is just flag
        let mut out = Vec::with_capacity(1 + nib_vec.len() / 2);
        out.push(flag);
        for chunk in nib_vec.chunks(2) {
            out.push((chunk[0] << 4) | chunk[1]);
        }
        out
    }
}

/// Decode hex-prefix back to nibbles + is_leaf flag.
pub fn decode_path(compact: &[u8]) -> Result<(Nibbles, bool), &'static str> {
    if compact.is_empty() {
        return Err("empty compact path");
    }

    let first = compact[0];
    let is_leaf = (first & 0x20) != 0;
    let is_odd = (first & 0x10) != 0;

    let mut nibs = Vec::new();
    if is_odd {
        nibs.push(first & 0x0f);
    }
    for &b in &compact[1..] {
        nibs.push(b >> 4);
        nibs.push(b & 0x0f);
    }
    Ok((Nibbles::from_nibbles(&nibs), is_leaf))
}

/// Encode a leaf node into an existing buffer: RLP([compact_path, value])
///
/// The caller must `clear()` `buf` before calling if it needs a clean slate.
pub fn encode_leaf_into(buf: &mut Vec<u8>, nibbles: &Nibbles, value: &[u8]) {
    let path_rlp_len = path_compact_rlp_len(nibbles.len());
    let payload_len = path_rlp_len + rlp_bytes_len(value);
    Header { list: true, payload_length: payload_len }.encode(buf);
    write_path_rlp(buf, nibbles, true);
    encode_bytes(value, buf);
}

/// Encode an extension node into an existing buffer: RLP([compact_path, child_rlp_or_hash])
///
/// The caller must `clear()` `buf` before calling if it needs a clean slate.
pub fn encode_extension_into(buf: &mut Vec<u8>, nibbles: &Nibbles, child_rlp_or_hash: &[u8]) {
    let path_rlp_len = path_compact_rlp_len(nibbles.len());
    let payload_len = path_rlp_len + child_embed_len(child_rlp_or_hash);
    Header { list: true, payload_length: payload_len }.encode(buf);
    write_path_rlp(buf, nibbles, false);
    embed_child_bytes(child_rlp_or_hash, buf);
}

/// Encode a branch node into an existing buffer: RLP([child0, ..., child15, value])
///
/// children_bytes: each child's embedding bytes (inline RLP or 32-byte hash), None = empty (0x80)
/// value: branch node's own value, None = empty
/// The caller must `clear()` `buf` before calling if it needs a clean slate.
pub fn encode_branch_into(
    buf: &mut Vec<u8>,
    children_bytes: &[Option<Vec<u8>>; 16],
    value: Option<&[u8]>,
) {
    let mut payload_len = 0usize;
    for child in children_bytes {
        match child {
            None => payload_len += 1, // 0x80 = 1 byte
            Some(bytes) => payload_len += child_embed_len(bytes),
        }
    }
    match value {
        None => payload_len += 1, // 0x80
        Some(v) => payload_len += rlp_bytes_len(v),
    }
    Header { list: true, payload_length: payload_len }.encode(buf);
    for child in children_bytes {
        match child {
            None => buf.push(0x80),
            Some(bytes) => embed_child_bytes(bytes, buf),
        }
    }
    match value {
        None => buf.push(0x80),
        Some(v) => encode_bytes(v, buf),
    }
}

/// Encode a leaf node: RLP([compact_path, value])
pub fn encode_leaf(nibbles: &Nibbles, value: &[u8]) -> Vec<u8> {
    let mut buf = Vec::new();
    encode_leaf_into(&mut buf, nibbles, value);
    buf
}

/// Encode an extension node: RLP([compact_path, child_rlp_or_hash])
pub fn encode_extension(nibbles: &Nibbles, child_rlp_or_hash: &[u8]) -> Vec<u8> {
    let mut buf = Vec::new();
    encode_extension_into(&mut buf, nibbles, child_rlp_or_hash);
    buf
}

/// Encode a branch node: RLP([child0, child1, ..., child15, value])
///
/// children_bytes: each child's embedding bytes (inline RLP or 32-byte hash), None = empty (0x80)
/// value: branch node's own value, None = empty
pub fn encode_branch(children_bytes: &[Option<Vec<u8>>; 16], value: Option<&[u8]>) -> Vec<u8> {
    let mut buf = Vec::new();
    encode_branch_into(&mut buf, children_bytes, value);
    buf
}

/// Encode node RLP as a child reference (inline or hash).
pub fn encode_child_ref(rlp_bytes: &[u8]) -> Vec<u8> {
    if should_inline(rlp_bytes) {
        rlp_bytes.to_vec()
    } else {
        hash_rlp(rlp_bytes).to_vec()
    }
}

/// Decode RLP bytes into an MptNode.
pub fn decode_node(rlp_bytes: &[u8]) -> Result<MptNode, &'static str> {
    let mut buf = rlp_bytes;
    let header = Header::decode(&mut buf).map_err(|_| "invalid RLP header")?;
    if !header.list {
        return Err("expected RLP list for node");
    }

    let payload_start = rlp_bytes.len() - buf.len();
    let payload = &rlp_bytes[payload_start..payload_start + header.payload_length];
    let elements = decode_list_elements(payload)?;

    if elements.len() == 17 {
        let mut children: [Option<ChildRef>; 16] = std::array::from_fn(|_| None);
        for i in 0..16 {
            children[i] = decode_child_element(&elements[i])?;
        }
        let value = if elements[16].is_empty() || elements[16] == [0x80] {
            None
        } else {
            Some(decode_rlp_bytes(&elements[16])?)
        };
        Ok(MptNode::Branch(BranchNode { children, value }))
    } else if elements.len() == 2 {
        let path_bytes = decode_rlp_bytes(&elements[0])?;
        let (nibbles, is_leaf) = decode_path(&path_bytes)?;
        if is_leaf {
            let value = decode_rlp_bytes(&elements[1])?;
            Ok(MptNode::Leaf(LeafNode { nibbles, value }))
        } else {
            let child =
                decode_child_element(&elements[1])?.ok_or("extension node must have a child")?;
            Ok(MptNode::Extension(ExtensionNode { nibbles, child }))
        }
    } else {
        Err("invalid RLP list length for MPT node")
    }
}

// ── Internal helpers ──

fn encode_bytes(data: &[u8], buf: &mut Vec<u8>) {
    data.as_ref().encode(buf);
}

fn rlp_bytes_len(data: &[u8]) -> usize {
    let slice: &[u8] = data;
    slice.length()
}

/// RLP-encoded byte length of a compact path with `nib_len` nibbles.
///
/// Compact encoding is always `1 + nib_len / 2` bytes.  The first byte is
/// always a flag (0x00/0x10/0x20/0x30 | optional nibble), so always < 0x80.
/// RLP rules for byte strings:
/// - compact_len == 1: single byte < 0x80 → no header, 1 byte total.
/// - compact_len  > 1: 0x80 + len header → 1 + compact_len bytes total.
fn path_compact_rlp_len(nib_len: usize) -> usize {
    let compact_len = 1 + nib_len / 2;
    if compact_len == 1 {
        1
    } else {
        1 + compact_len
    }
}

/// Write RLP-encoded compact path directly into `buf` — no intermediate Vec.
///
/// Equivalent to `encode_bytes(&encode_path(nibbles, is_leaf), buf)` but
/// avoids all intermediate allocations.  Uses `Nibbles::iter()` to stream
/// nibble values without calling `to_vec()`.
///
/// # Panics (debug only)
/// Asserts `compact_len <= 55` — valid for all Ethereum key lengths (max 64
/// nibbles → compact_len 33).  Compact paths exceeding 55 bytes would require
/// a long-form RLP string header that this function does not implement.
fn write_path_rlp(buf: &mut Vec<u8>, nibbles: &Nibbles, is_leaf: bool) {
    let flag = if is_leaf { 0x20u8 } else { 0x00u8 };
    let nib_len = nibbles.len();
    let compact_len = 1 + nib_len / 2;
    // Ethereum compact paths are at most 33 bytes (64 nibbles → compact_len 33).
    // Long-form RLP string header (payload > 55 bytes) is not implemented here.
    debug_assert!(compact_len <= 55, "compact_len {compact_len} exceeds short-string RLP limit");
    // Write RLP string header (omitted for single byte < 0x80).
    if compact_len > 1 {
        buf.push(0x80 + compact_len as u8);
    }
    // Stream nibbles via iterator — zero allocation.
    let mut iter = nibbles.iter();
    if nib_len % 2 == 1 {
        // Odd: first byte encodes flag + first nibble.
        let first = iter.next().unwrap_or(0);
        buf.push(flag | 0x10 | first);
    } else {
        // Even: first byte is just the flag.
        buf.push(flag);
    }
    // Pack remaining nibbles in pairs.
    while let Some(hi) = iter.next() {
        let lo = iter.next().unwrap_or(0);
        buf.push((hi << 4) | lo);
    }
}

/// Length of an embedded child in parent's RLP.
/// 32-byte hash → RLP bytes string (33 bytes: 0xa0 header + 32).
/// Otherwise → inline RLP, embedded raw.
fn child_embed_len(child_bytes: &[u8]) -> usize {
    if child_bytes.len() == 32 {
        33 // 1 byte header (0xa0) + 32 bytes
    } else {
        child_bytes.len()
    }
}

/// Write child bytes into parent's RLP buffer.
/// 32-byte hash → encode as RLP byte string. Otherwise → embed raw.
fn embed_child_bytes(child_bytes: &[u8], buf: &mut Vec<u8>) {
    if child_bytes.len() == 32 {
        child_bytes.as_ref().encode(buf);
    } else {
        buf.extend_from_slice(child_bytes);
    }
}

fn decode_list_elements(mut payload: &[u8]) -> Result<Vec<Vec<u8>>, &'static str> {
    let mut elements = Vec::new();
    while !payload.is_empty() {
        let start = payload;
        let header = Header::decode(&mut payload).map_err(|_| "invalid RLP element header")?;
        let elem_start = start.len() - payload.len();
        if header.list {
            let mut elem = Vec::new();
            header.encode(&mut elem);
            elem.extend_from_slice(&payload[..header.payload_length]);
            payload = &payload[header.payload_length..];
            elements.push(elem);
        } else {
            let total_len = elem_start + header.payload_length;
            let full_elem = &start[..total_len];
            payload = &payload[header.payload_length..];
            elements.push(full_elem.to_vec());
        }
    }
    Ok(elements)
}

fn decode_rlp_bytes(element: &[u8]) -> Result<Vec<u8>, &'static str> {
    let mut buf: &[u8] = element;
    let bytes = alloy_rlp::Bytes::decode(&mut buf).map_err(|_| "invalid RLP bytes")?;
    Ok(bytes.to_vec())
}

fn decode_child_element(element: &[u8]) -> Result<Option<ChildRef>, &'static str> {
    if element.is_empty() || element == [0x80] {
        return Ok(None);
    }

    let mut buf: &[u8] = element;
    let header = Header::decode(&mut buf).map_err(|_| "invalid child RLP")?;

    if header.list {
        if element.len() > 32 {
            return Err("inline child RLP exceeds 32 bytes");
        }
        Ok(Some(ChildRef::Inline(element.to_vec())))
    } else {
        let data = &buf[..header.payload_length];
        if data.is_empty() {
            Ok(None)
        } else if data.len() == 32 {
            Ok(Some(ChildRef::Hash(B256::from_slice(data))))
        } else if data.len() < 32 {
            Ok(Some(ChildRef::Inline(element.to_vec())))
        } else {
            Err("child bytes exceed 32 bytes")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn t2_1_encode_path_odd_leaf() {
        let nibs = Nibbles::from_nibbles(&[0x1, 0x2, 0x3]);
        let result = encode_path(&nibs, true);
        assert_eq!(result, vec![0x31, 0x23]);
    }

    #[test]
    fn t2_2_encode_path_even_extension() {
        let nibs = Nibbles::from_nibbles(&[0x1, 0x2, 0x3, 0x4]);
        let result = encode_path(&nibs, false);
        assert_eq!(result, vec![0x00, 0x12, 0x34]);
    }

    #[test]
    fn t2_3_encode_path_empty_leaf() {
        let nibs = Nibbles::from_nibbles(&[]);
        let result = encode_path(&nibs, true);
        assert_eq!(result, vec![0x20]);
    }

    #[test]
    fn t2_4_leaf_roundtrip() {
        let nibs = Nibbles::from_nibbles(&[1, 2, 3, 4, 5]);
        let value = b"hello".to_vec();
        let encoded = encode_leaf(&nibs, &value);
        let decoded = decode_node(&encoded).unwrap();
        match decoded {
            MptNode::Leaf(leaf) => {
                assert_eq!(leaf.nibbles, nibs);
                assert_eq!(leaf.value, value);
            }
            _ => panic!("expected Leaf"),
        }
    }

    #[test]
    fn t2_5_extension_roundtrip() {
        let nibs = Nibbles::from_nibbles(&[0xa, 0xb]);
        let child_hash = B256::repeat_byte(0xaa);
        let encoded = encode_extension(&nibs, child_hash.as_slice());
        let decoded = decode_node(&encoded).unwrap();
        match decoded {
            MptNode::Extension(ext) => {
                assert_eq!(ext.nibbles, nibs);
                match ext.child {
                    ChildRef::Hash(h) => assert_eq!(h, child_hash),
                    _ => panic!("expected Hash child"),
                }
            }
            _ => panic!("expected Extension"),
        }
    }

    #[test]
    fn t2_6_branch_roundtrip() {
        let mut children: [Option<Vec<u8>>; 16] = std::array::from_fn(|_| None);
        let hash = B256::repeat_byte(0xbb);
        children[3] = Some(hash.as_slice().to_vec());
        let inline_rlp = vec![0xc1, 0x80];
        children[7] = Some(inline_rlp.clone());

        let value = Some(b"branch_val".as_ref());
        let encoded = encode_branch(&children, value);
        let decoded = decode_node(&encoded).unwrap();
        match decoded {
            MptNode::Branch(b) => {
                match b.children[3].as_ref().unwrap() {
                    ChildRef::Hash(h) => assert_eq!(*h, hash),
                    _ => panic!("expected Hash at index 3"),
                }
                match b.children[7].as_ref().unwrap() {
                    ChildRef::Inline(rlp) => assert_eq!(*rlp, inline_rlp),
                    _ => panic!("expected Inline at index 7"),
                }
                for i in 0..16 {
                    if i != 3 && i != 7 {
                        assert!(b.children[i].is_none(), "child {i} should be None");
                    }
                }
                assert_eq!(b.value.as_deref(), Some(b"branch_val".as_ref()));
            }
            _ => panic!("expected Branch"),
        }
    }

    #[test]
    fn t2_7_should_inline_boundary() {
        assert!(should_inline(&[0u8; 31]));
        assert!(!should_inline(&[0u8; 32]));
    }

    #[test]
    fn t2_8_alloy_rlp_bytes_compat() {
        let value = b"test";
        let path = encode_path(&Nibbles::from_nibbles(&[1, 2]), true);

        let mut expected = Vec::new();
        let payload_len = rlp_bytes_len(&path) + rlp_bytes_len(value);
        Header { list: true, payload_length: payload_len }.encode(&mut expected);
        path.as_slice().encode(&mut expected);
        value.as_slice().encode(&mut expected);

        let our_encoded = encode_leaf(&Nibbles::from_nibbles(&[1, 2]), value);
        assert_eq!(our_encoded, expected);
    }

    #[test]
    fn t2_9_decode_path_empty_input() {
        assert!(decode_path(&[]).is_err());
    }

    #[test]
    fn t2_10_decode_branch_child_types() {
        let mut children: [Option<Vec<u8>>; 16] = std::array::from_fn(|_| None);
        let hash = B256::repeat_byte(0xcc);
        children[0] = Some(hash.as_slice().to_vec());
        let short_rlp = vec![0xc2, 0x80, 0x80];
        children[1] = Some(short_rlp);

        let encoded = encode_branch(&children, None);
        let decoded = decode_node(&encoded).unwrap();
        match decoded {
            MptNode::Branch(b) => {
                match b.children[0].as_ref().unwrap() {
                    ChildRef::Hash(h) => assert_eq!(*h, hash),
                    other => panic!("expected Hash, got {other:?}"),
                }
                match b.children[1].as_ref().unwrap() {
                    ChildRef::Inline(_) => {}
                    other => panic!("expected Inline, got {other:?}"),
                }
                assert!(b.children[2].is_none());
            }
            _ => panic!("expected Branch"),
        }
    }

    #[test]
    fn t2_11_decode_node_type_detection() {
        let children: [Option<Vec<u8>>; 16] = std::array::from_fn(|_| None);
        let encoded = encode_branch(&children, None);
        assert!(decode_node(&encoded).unwrap().is_branch());

        let encoded = encode_leaf(&Nibbles::from_nibbles(&[1]), b"val");
        assert!(decode_node(&encoded).unwrap().is_leaf());

        let hash = B256::repeat_byte(0xdd);
        let encoded = encode_extension(&Nibbles::from_nibbles(&[1, 2]), hash.as_slice());
        assert!(decode_node(&encoded).unwrap().is_extension());
    }

    #[test]
    fn t2_12_decode_node_child_too_long() {
        let mut long_bytes = vec![0xb8, 33];
        long_bytes.extend_from_slice(&[0xaa; 33]);
        let result = decode_child_element(&long_bytes);
        assert!(result.is_err());
    }

    #[test]
    fn t2_13_extension_child_decode() {
        let hash = B256::repeat_byte(0xee);
        let encoded = encode_extension(&Nibbles::from_nibbles(&[5]), hash.as_slice());
        match decode_node(&encoded).unwrap() {
            MptNode::Extension(ext) => match ext.child {
                ChildRef::Hash(h) => assert_eq!(h, hash),
                _ => panic!("expected Hash child"),
            },
            _ => panic!("expected Extension"),
        }

        let short_rlp = vec![0xc2, 0x80, 0x80];
        let encoded = encode_extension(&Nibbles::from_nibbles(&[5]), &short_rlp);
        match decode_node(&encoded).unwrap() {
            MptNode::Extension(ext) => match ext.child {
                ChildRef::Inline(_) => {}
                _ => panic!("expected Inline child"),
            },
            _ => panic!("expected Extension"),
        }
    }

    #[test]
    fn t2_14_encode_child_ref_inline_vs_hash() {
        let short_rlp = vec![0xc1, 0x80];
        let result = encode_child_ref(&short_rlp);
        assert_eq!(result, short_rlp);

        let long_rlp = vec![0xaa; 40];
        let result = encode_child_ref(&long_rlp);
        assert_eq!(result.len(), 32);
        assert_eq!(B256::from_slice(&result), hash_rlp(&long_rlp));
    }
}
