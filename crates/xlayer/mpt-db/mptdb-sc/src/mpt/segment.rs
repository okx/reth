use alloy_primitives::B256;
use alloy_trie::Nibbles;
use memmap2::Mmap;
use mptdb_common::error::{MptDbError, Result};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use super::{
    arena::MutableTrieArena,
    encoding::{encode_branch, encode_extension, encode_leaf},
    flat_layout::{encode_page, read_page_header, FlatPageHeader, FLAT_PAGE_HEADER_LEN},
    hash,
    node::{BranchNode, ChildRef, ExtensionNode, LeafNode, MptNode},
    tree::MptTree,
};

const SEGMENT_MAGIC: &[u8; 8] = b"stsegx01";
const SEGMENT_VERSION: u16 = 1;
const SEGMENT_HEADER_LEN: usize = 60;
const NODE_RECORD_LEN: usize = 60;
const CHILD_RECORD_LEN: usize = 48;

const TAG_BRANCH: u8 = 1;
const TAG_EXTENSION: u8 = 2;
const TAG_LEAF: u8 = 3;

const EMBED_NONE: u8 = 0;
const EMBED_HASH: u8 = 1;
const EMBED_INLINE: u8 = 2;

pub const STORAGE_SEGMENT_FORMAT_VERSION: u16 = SEGMENT_VERSION;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StorageSegmentLocator {
    pub root: B256,
    pub page_off: u64,
    pub record_off: u32,
    pub format_version: u16,
}

impl StorageSegmentLocator {
    pub fn new(root: B256, page_off: u64, record_off: u32) -> Self {
        Self { root, page_off, record_off, format_version: STORAGE_SEGMENT_FORMAT_VERSION }
    }
}

#[derive(Clone)]
enum ChildEmbedOwned {
    Hash(B256),
    Inline(Vec<u8>),
}

struct BuildChild {
    slot: u8,
    target_idx: Option<u32>,
    embed: ChildEmbedOwned,
}

enum BuildNode {
    Branch { value: Option<Vec<u8>>, hash: Option<B256>, children: Vec<BuildChild> },
    Extension { nibbles: Vec<u8>, hash: Option<B256>, child: BuildChild },
    Leaf { nibbles: Vec<u8>, value: Vec<u8>, hash: Option<B256> },
}

#[derive(Clone)]
pub struct StorageTrieSegment {
    bytes: Vec<u8>,
    root: B256,
    root_record_off: u32,
}

impl StorageTrieSegment {
    pub fn from_tree(tree: &MptTree, root: B256) -> Result<Self> {
        let nodes = tree.arena_nodes();
        let hash_cache = tree.arena_hash_cache();
        Self::from_parts(&nodes, &hash_cache, tree.root_index(), root)
    }

    pub(crate) fn from_parts(
        nodes: &[MptNode],
        hash_cache: &[Option<B256>],
        root_idx: Option<u32>,
        root: B256,
    ) -> Result<Self> {
        let Some(root_idx) = root_idx else {
            return Err(MptDbError::Other("segment requires non-empty trie".to_string()));
        };

        let mut reachable = Vec::new();
        let mut arena_to_segment = vec![None; nodes.len()];
        collect_reachable_nodes(nodes, root_idx, &mut arena_to_segment, &mut reachable);

        let mut build_nodes = Vec::with_capacity(reachable.len());
        for &arena_idx in &reachable {
            build_nodes.push(build_node(nodes, hash_cache, arena_idx, &arena_to_segment)?);
        }

        let payload = encode_segment(&build_nodes, root, 0);
        let root_record_off = (FLAT_PAGE_HEADER_LEN + SEGMENT_HEADER_LEN) as u32;
        Ok(Self { bytes: encode_page(&payload, root, root_record_off), root, root_record_off })
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub fn root(&self) -> B256 {
        self.root
    }

    pub fn root_record_off(&self) -> u32 {
        self.root_record_off
    }

    pub fn into_page_lease(self) -> Arc<SegmentPageLease> {
        let root = self.root;
        let root_record_off = self.root_record_off;
        let bytes: Arc<[u8]> = self.bytes.into();
        let mapped = Arc::new(MappedSegmentPage::from_owned_bytes(bytes));
        Arc::new(SegmentPageLease::new(mapped, root, root_record_off))
    }
}

pub struct StorageTrieSegmentReader<'a> {
    bytes: &'a [u8],
    node_count: u32,
    child_count: u32,
    root_idx: u32,
    blob_offset: usize,
    root: B256,
    lease: Option<Arc<SegmentPageLease>>,
}

pub struct StoragePathCursor<'a> {
    reader: &'a StorageTrieSegmentReader<'a>,
}

pub struct StoragePathTrace {
    arena: MutableTrieArena,
    root: Option<u32>,
    touched_keys: usize,
}

#[derive(Clone)]
pub struct MappedSegmentPage {
    backing: SegmentPageBacking,
    page_off: usize,
    total_len: usize,
}

#[derive(Clone)]
enum SegmentPageBacking {
    Mmap(Arc<Mmap>),
    Owned(Arc<[u8]>),
}

impl MappedSegmentPage {
    pub fn new(mmap: Arc<Mmap>, page_off: usize, total_len: usize) -> Self {
        Self { backing: SegmentPageBacking::Mmap(mmap), page_off, total_len }
    }

    pub fn from_owned_bytes(bytes: Arc<[u8]>) -> Self {
        let total_len = bytes.len();
        Self { backing: SegmentPageBacking::Owned(bytes), page_off: 0, total_len }
    }

    pub fn as_slice(&self) -> &[u8] {
        match &self.backing {
            SegmentPageBacking::Mmap(mmap) => &mmap[self.page_off..self.page_off + self.total_len],
            SegmentPageBacking::Owned(bytes) => {
                &bytes[self.page_off..self.page_off + self.total_len]
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn as_ptr(&self) -> *const u8 {
        self.as_slice().as_ptr()
    }
}

#[derive(Clone)]
pub struct SegmentPageLease {
    page: Arc<MappedSegmentPage>,
    root: B256,
    root_record_off: u32,
}

impl SegmentPageLease {
    pub fn new(page: Arc<MappedSegmentPage>, root: B256, root_record_off: u32) -> Self {
        Self { page, root, root_record_off }
    }

    pub fn as_slice(&self) -> &[u8] {
        self.page.as_slice()
    }

    pub fn root(&self) -> B256 {
        self.root
    }

    pub fn root_record_off(&self) -> u32 {
        self.root_record_off
    }

    #[cfg(test)]
    pub(crate) fn as_ptr(&self) -> *const u8 {
        self.page.as_ptr()
    }
}

#[derive(Clone)]
pub struct SegmentNodeRef {
    lease: Arc<SegmentPageLease>,
    seg_idx: u32,
    hash: Option<B256>,
}

impl SegmentNodeRef {
    pub fn new(lease: Arc<SegmentPageLease>, seg_idx: u32, hash: Option<B256>) -> Self {
        Self { lease, seg_idx, hash }
    }

    pub fn page_lease(&self) -> &Arc<SegmentPageLease> {
        &self.lease
    }

    pub fn seg_idx(&self) -> u32 {
        self.seg_idx
    }

    pub fn hash(&self) -> Option<B256> {
        self.hash
    }
}

#[derive(Clone, Copy)]
pub struct SegmentChildrenView<'a> {
    reader: &'a StorageTrieSegmentReader<'a>,
    start_idx: usize,
    len: usize,
}

impl<'a> SegmentChildrenView<'a> {
    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn get(&self, idx: usize) -> Result<SegmentChildView<'a>> {
        if idx >= self.len {
            return Err(MptDbError::Other(format!("segment child view out of bounds: {idx}")));
        }
        self.reader.decode_child_view(self.start_idx + idx)
    }

    pub fn iter(&self) -> impl Iterator<Item = Result<SegmentChildView<'a>>> + '_ {
        (0..self.len).map(move |idx| self.get(idx))
    }
}

#[derive(Clone, Copy)]
pub enum SegmentChildEmbedRef<'a> {
    None,
    Hash(B256),
    Inline(&'a [u8]),
}

#[derive(Clone, Copy)]
pub struct SegmentChildView<'a> {
    pub slot: u8,
    pub target_idx: Option<u32>,
    pub embed: SegmentChildEmbedRef<'a>,
}

pub enum SegmentNodeKind<'a> {
    Branch { child_bitmap: u16, value: Option<&'a [u8]>, children: SegmentChildrenView<'a> },
    Extension { nibbles: &'a [u8], child: SegmentChildView<'a> },
    Leaf { nibbles: &'a [u8], value: &'a [u8] },
}

pub struct SegmentNodeView<'a> {
    pub kind: SegmentNodeKind<'a>,
    pub hash: Option<B256>,
}

impl StoragePathTrace {
    pub fn into_parts(self) -> (MutableTrieArena, Option<u32>) {
        (self.arena, self.root)
    }

    #[cfg(test)]
    pub fn into_tree(self) -> MptTree {
        MptTree { arena: self.arena, root: self.root }
    }

    pub fn touched_keys(&self) -> usize {
        self.touched_keys
    }
}

impl<'a> StorageTrieSegmentReader<'a> {
    pub fn open_shared_page(
        lease: &'a Arc<SegmentPageLease>,
        expected_root: B256,
        expected_record_off: u32,
    ) -> Result<Self> {
        let mut reader = Self::open_page(lease.as_slice(), expected_root, expected_record_off)?;
        reader.lease = Some(Arc::clone(lease));
        Ok(reader)
    }

    pub fn open_page(
        bytes: &'a [u8],
        expected_root: B256,
        expected_record_off: u32,
    ) -> Result<Self> {
        let page = read_page_header(bytes)?;
        if page.root != expected_root {
            return Err(MptDbError::Other("flat page root mismatch".to_string()));
        }
        if page.root_record_off != expected_record_off {
            return Err(MptDbError::Other("flat page record offset mismatch".to_string()));
        }
        let page_end = page.total_len as usize;
        let payload = bytes
            .get(page.payload_off as usize..page_end)
            .ok_or_else(|| MptDbError::Other("flat page payload out of bounds".to_string()))?;
        Self::open_payload(payload, expected_root, page)
    }

    pub fn open(bytes: &'a [u8], expected_root: B256) -> Result<Self> {
        let page = read_page_header(bytes)?;
        Self::open_page(bytes, expected_root, page.root_record_off)
    }

    fn open_payload(bytes: &'a [u8], expected_root: B256, _page: FlatPageHeader) -> Result<Self> {
        if bytes.len() < SEGMENT_HEADER_LEN {
            return Err(MptDbError::Other("segment too short".to_string()));
        }
        if &bytes[..8] != SEGMENT_MAGIC {
            return Err(MptDbError::Other("invalid segment magic".to_string()));
        }
        let version = read_u16(bytes, 8)?;
        if version != SEGMENT_VERSION {
            return Err(MptDbError::Other(format!("unsupported segment version: {version}")));
        }

        let node_count = read_u32(bytes, 12)?;
        let child_count = read_u32(bytes, 16)?;
        let root_idx = read_u32(bytes, 20)?;
        let blob_offset = read_u32(bytes, 24)? as usize;
        let root = read_b256(bytes, 28)?;
        if root != expected_root {
            return Err(MptDbError::Other("segment root mismatch".to_string()));
        }

        if blob_offset > bytes.len() {
            return Err(MptDbError::Other("segment blob offset out of bounds".to_string()));
        }

        Ok(Self { bytes, node_count, child_count, root_idx, blob_offset, root, lease: None })
    }

    pub fn root(&self) -> B256 {
        self.root
    }

    pub fn root_ref(&self) -> Result<SegmentNodeRef> {
        let lease = self.lease.as_ref().ok_or_else(|| {
            MptDbError::Other("segment root ref requires shared page lease".to_string())
        })?;
        let hash = self.view_node(self.root_idx)?.hash;
        Ok(SegmentNodeRef { lease: Arc::clone(lease), seg_idx: self.root_idx, hash })
    }

    pub fn view_node(&self, seg_idx: u32) -> Result<SegmentNodeView<'_>> {
        if seg_idx >= self.node_count {
            return Err(MptDbError::Other(format!("segment node idx out of bounds: {seg_idx}")));
        }
        let off = SEGMENT_HEADER_LEN + seg_idx as usize * NODE_RECORD_LEN;
        let rec = self.bytes.get(off..off + NODE_RECORD_LEN).ok_or_else(|| {
            MptDbError::Other(format!("segment node record out of bounds: {seg_idx}"))
        })?;

        let tag = rec[0];
        let flags = rec[1];
        let child_meta_count = u16::from_le_bytes([rec[2], rec[3]]) as usize;
        let child_meta_offset = u32::from_le_bytes([rec[4], rec[5], rec[6], rec[7]]) as usize;
        let path_offset = u32::from_le_bytes([rec[8], rec[9], rec[10], rec[11]]) as usize;
        let path_len = u16::from_le_bytes([rec[12], rec[13]]) as usize;
        let value_offset = u32::from_le_bytes([rec[16], rec[17], rec[18], rec[19]]) as usize;
        let value_len = u32::from_le_bytes([rec[20], rec[21], rec[22], rec[23]]) as usize;
        let hash = if (flags & 0b10) != 0 { Some(B256::from_slice(&rec[24..56])) } else { None };

        let path = if path_len == 0 { &[][..] } else { self.blob_slice(path_offset, path_len)? };
        let kind = match tag {
            TAG_BRANCH => {
                let child_bitmap = u16::from_le_bytes([rec[14], rec[15]]);
                let value = if (flags & 0b01) != 0 {
                    Some(self.blob_slice(value_offset, value_len)?)
                } else {
                    None
                };
                SegmentNodeKind::Branch {
                    child_bitmap,
                    value,
                    children: SegmentChildrenView {
                        reader: self,
                        start_idx: child_meta_offset,
                        len: child_meta_count,
                    },
                }
            }
            TAG_EXTENSION => {
                let child = self.decode_child_view(child_meta_offset)?;
                SegmentNodeKind::Extension { nibbles: path, child }
            }
            TAG_LEAF => SegmentNodeKind::Leaf {
                nibbles: path,
                value: self.blob_slice(value_offset, value_len)?,
            },
            _ => return Err(MptDbError::Other(format!("unknown segment node tag: {tag}"))),
        };
        Ok(SegmentNodeView { kind, hash })
    }

    pub fn segment_child_ref(&self, seg_idx: u32, slot: usize) -> Result<Option<SegmentNodeRef>> {
        let lease = match self.lease.as_ref() {
            Some(lease) => lease,
            None => return Ok(None),
        };
        let target_idx = match self.view_node(seg_idx)?.kind {
            SegmentNodeKind::Branch { children, .. } => {
                let mut target = None;
                for child in children.iter() {
                    let child = child?;
                    if child.slot as usize == slot {
                        target = child.target_idx;
                        break;
                    }
                }
                target
            }
            SegmentNodeKind::Extension { child, .. } if slot == 0 => child.target_idx,
            _ => None,
        };
        let Some(target_idx) = target_idx else {
            return Ok(None);
        };
        let hash = self.view_node(target_idx)?.hash;
        Ok(Some(SegmentNodeRef { lease: Arc::clone(lease), seg_idx: target_idx, hash }))
    }

    pub fn cursor(&'a self) -> StoragePathCursor<'a> {
        StoragePathCursor { reader: self }
    }

    pub fn trace_touched_paths(&self, keys: &[Nibbles]) -> Result<StoragePathTrace> {
        if self.root_idx == u32::MAX {
            return Ok(StoragePathTrace {
                arena: MutableTrieArena::new(),
                root: None,
                touched_keys: keys.len(),
            });
        }

        let mut arena = MutableTrieArena::new();
        let mut materialized = vec![None; self.node_count as usize];

        for key in keys {
            self.materialize_for_key(self.root_idx, key, 0, &mut arena, &mut materialized)?;
        }

        let root_idx =
            materialized.get(self.root_idx as usize).copied().flatten().ok_or_else(|| {
                MptDbError::Other("segment root was not materialized".to_string())
            })?;
        Ok(StoragePathTrace { arena, root: Some(root_idx), touched_keys: keys.len() })
    }

    #[cfg(test)]
    pub fn materialize_touched_paths(&self, keys: &[Nibbles]) -> Result<MptTree> {
        Ok(self.trace_touched_paths(keys)?.into_tree())
    }

    fn materialize_for_key(
        &self,
        seg_idx: u32,
        key: &Nibbles,
        offset: usize,
        arena: &mut MutableTrieArena,
        materialized: &mut [Option<u32>],
    ) -> Result<u32> {
        let mut inserted = false;
        let arena_idx = if let Some(idx) = materialized.get(seg_idx as usize).copied().flatten() {
            idx
        } else {
            let node = self.decode_node(seg_idx)?;
            let idx = arena.alloc_clean(node.node);
            if let Some(hash) = node.hash {
                arena.set_hash(idx, hash);
            }
            materialized[seg_idx as usize] = Some(idx);
            inserted = true;
            idx
        };

        let node = self.decode_node(seg_idx)?;
        match node.body {
            SegmentNodeBody::Leaf { .. } => Ok(arena_idx),
            SegmentNodeBody::Extension { nibbles, child } => {
                let remaining = key.slice(offset..);
                if remaining.len() < nibbles.len() || remaining.slice(..nibbles.len()) != nibbles {
                    return Ok(arena_idx);
                }
                if let Some(target_idx) = child.target_idx {
                    let child_arena = self.materialize_for_key(
                        target_idx,
                        key,
                        offset + nibbles.len(),
                        arena,
                        materialized,
                    )?;
                    if inserted ||
                        !matches!(
                            self.current_extension_child(arena, arena_idx),
                            ChildRef::Arena(_)
                        )
                    {
                        if let MptNode::Extension(ext) = arena.get_mut(arena_idx) {
                            ext.child = ChildRef::Arena(child_arena);
                        }
                    }
                }
                Ok(arena_idx)
            }
            SegmentNodeBody::Branch { child_bitmap, value: _, children } => {
                if offset >= key.len() {
                    return Ok(arena_idx);
                }
                let nibble = key.get_unchecked(offset);
                if (child_bitmap & (1u16 << nibble)) == 0 {
                    return Ok(arena_idx);
                }
                if let Some(child) = children.iter().find(|child| child.slot == nibble as u8) {
                    if let Some(target_idx) = child.target_idx {
                        let child_arena = self.materialize_for_key(
                            target_idx,
                            key,
                            offset + 1,
                            arena,
                            materialized,
                        )?;
                        let current = self.current_branch_child(arena, arena_idx, nibble as usize);
                        if inserted || !matches!(current, Some(ChildRef::Arena(_))) {
                            if let MptNode::Branch(branch) = arena.get_mut(arena_idx) {
                                branch.children[nibble as usize] =
                                    Some(ChildRef::Arena(child_arena));
                            }
                        }
                    }
                }
                Ok(arena_idx)
            }
        }
    }

    fn current_extension_child(&self, arena: &MutableTrieArena, arena_idx: u32) -> ChildRef {
        match arena.get(arena_idx) {
            MptNode::Extension(ext) => ext.child.clone(),
            _ => ChildRef::Hash(B256::ZERO),
        }
    }

    fn current_branch_child(
        &self,
        arena: &MutableTrieArena,
        arena_idx: u32,
        slot: usize,
    ) -> Option<ChildRef> {
        match arena.get(arena_idx) {
            MptNode::Branch(branch) => branch.children[slot].clone(),
            _ => None,
        }
    }

    fn decode_node(&self, seg_idx: u32) -> Result<DecodedSegmentNode> {
        if seg_idx >= self.node_count {
            return Err(MptDbError::Other(format!("segment node idx out of bounds: {seg_idx}")));
        }
        let off = SEGMENT_HEADER_LEN + seg_idx as usize * NODE_RECORD_LEN;
        let rec = self.bytes.get(off..off + NODE_RECORD_LEN).ok_or_else(|| {
            MptDbError::Other(format!("segment node record out of bounds: {seg_idx}"))
        })?;

        let tag = rec[0];
        let flags = rec[1];
        let child_meta_count = u16::from_le_bytes([rec[2], rec[3]]) as usize;
        let child_meta_offset = u32::from_le_bytes([rec[4], rec[5], rec[6], rec[7]]) as usize;
        let path_offset = u32::from_le_bytes([rec[8], rec[9], rec[10], rec[11]]) as usize;
        let path_len = u16::from_le_bytes([rec[12], rec[13]]) as usize;
        let value_offset = u32::from_le_bytes([rec[16], rec[17], rec[18], rec[19]]) as usize;
        let value_len = u32::from_le_bytes([rec[20], rec[21], rec[22], rec[23]]) as usize;
        let hash = if (flags & 0b10) != 0 { Some(B256::from_slice(&rec[24..56])) } else { None };

        let path = if path_len == 0 {
            Nibbles::from_nibbles(&[])
        } else {
            Nibbles::from_nibbles(self.blob_slice(path_offset, path_len)?)
        };

        let body = match tag {
            TAG_BRANCH => {
                let child_bitmap = u16::from_le_bytes([rec[14], rec[15]]);
                let value = if (flags & 0b01) != 0 {
                    Some(self.blob_slice(value_offset, value_len)?.to_vec())
                } else {
                    None
                };
                let mut children = Vec::with_capacity(child_meta_count);
                for idx in 0..child_meta_count {
                    children.push(self.decode_child_meta(child_meta_offset + idx)?);
                }
                SegmentNodeBody::Branch { child_bitmap, value, children }
            }
            TAG_EXTENSION => {
                let child = self.decode_child_meta(child_meta_offset)?;
                SegmentNodeBody::Extension { nibbles: path, child }
            }
            TAG_LEAF => SegmentNodeBody::Leaf {
                nibbles: path,
                value: self.blob_slice(value_offset, value_len)?.to_vec(),
            },
            _ => {
                return Err(MptDbError::Other(format!("unknown segment node tag: {tag}")));
            }
        };

        let node = match &body {
            SegmentNodeBody::Branch { value, children, .. } => {
                let mut branch = BranchNode::new();
                branch.value = value.clone();
                for child in children {
                    branch.children[child.slot as usize] = Some(child.to_child_ref()?);
                }
                MptNode::Branch(branch)
            }
            SegmentNodeBody::Extension { nibbles, child } => MptNode::Extension(ExtensionNode {
                nibbles: nibbles.clone(),
                child: child.to_child_ref()?,
            }),
            SegmentNodeBody::Leaf { nibbles, value } => {
                MptNode::Leaf(LeafNode { nibbles: nibbles.clone(), value: value.clone() })
            }
        };

        Ok(DecodedSegmentNode { node, body, hash })
    }

    fn decode_child_meta(&self, idx: usize) -> Result<SegmentChildMeta> {
        if idx >= self.child_count as usize {
            return Err(MptDbError::Other(format!("segment child idx out of bounds: {idx}")));
        }
        let off = SEGMENT_HEADER_LEN +
            self.node_count as usize * NODE_RECORD_LEN +
            idx * CHILD_RECORD_LEN;
        let rec = self.bytes.get(off..off + CHILD_RECORD_LEN).ok_or_else(|| {
            MptDbError::Other(format!("segment child record out of bounds: {idx}"))
        })?;
        let slot = rec[0];
        let kind = rec[1];
        let target_idx = u32::from_le_bytes([rec[4], rec[5], rec[6], rec[7]]);
        let inline_offset = u32::from_le_bytes([rec[8], rec[9], rec[10], rec[11]]) as usize;
        let inline_len = u32::from_le_bytes([rec[12], rec[13], rec[14], rec[15]]) as usize;
        let hash = B256::from_slice(&rec[16..48]);
        Ok(SegmentChildMeta {
            slot,
            target_idx: (target_idx != u32::MAX).then_some(target_idx),
            embed: match kind {
                EMBED_NONE => SegmentChildEmbed::None,
                EMBED_HASH => SegmentChildEmbed::Hash(hash),
                EMBED_INLINE => {
                    SegmentChildEmbed::Inline(self.blob_slice(inline_offset, inline_len)?.to_vec())
                }
                _ => {
                    return Err(MptDbError::Other(format!(
                        "unknown segment child embed kind: {kind}"
                    )));
                }
            },
        })
    }

    fn decode_child_view(&self, idx: usize) -> Result<SegmentChildView<'_>> {
        if idx >= self.child_count as usize {
            return Err(MptDbError::Other(format!("segment child idx out of bounds: {idx}")));
        }
        let off = SEGMENT_HEADER_LEN +
            self.node_count as usize * NODE_RECORD_LEN +
            idx * CHILD_RECORD_LEN;
        let rec = self.bytes.get(off..off + CHILD_RECORD_LEN).ok_or_else(|| {
            MptDbError::Other(format!("segment child record out of bounds: {idx}"))
        })?;
        let slot = rec[0];
        let kind = rec[1];
        let target_idx = u32::from_le_bytes([rec[4], rec[5], rec[6], rec[7]]);
        let inline_offset = u32::from_le_bytes([rec[8], rec[9], rec[10], rec[11]]) as usize;
        let inline_len = u32::from_le_bytes([rec[12], rec[13], rec[14], rec[15]]) as usize;
        let hash = B256::from_slice(&rec[16..48]);
        Ok(SegmentChildView {
            slot,
            target_idx: (target_idx != u32::MAX).then_some(target_idx),
            embed: match kind {
                EMBED_NONE => SegmentChildEmbedRef::None,
                EMBED_HASH => SegmentChildEmbedRef::Hash(hash),
                EMBED_INLINE => {
                    SegmentChildEmbedRef::Inline(self.blob_slice(inline_offset, inline_len)?)
                }
                _ => {
                    return Err(MptDbError::Other(format!(
                        "unknown segment child embed kind: {kind}"
                    )));
                }
            },
        })
    }

    fn blob_slice(&self, offset: usize, len: usize) -> Result<&'a [u8]> {
        let start = self.blob_offset + offset;
        let end = start.saturating_add(len);
        self.bytes
            .get(start..end)
            .ok_or_else(|| MptDbError::Other("segment blob slice out of bounds".to_string()))
    }
}

impl<'a> StoragePathCursor<'a> {
    pub fn trace_paths(&self, keys: &[Nibbles]) -> Result<StoragePathTrace> {
        self.reader.trace_touched_paths(keys)
    }
}

struct DecodedSegmentNode {
    node: MptNode,
    body: SegmentNodeBody,
    hash: Option<B256>,
}

enum SegmentNodeBody {
    Branch { child_bitmap: u16, value: Option<Vec<u8>>, children: Vec<SegmentChildMeta> },
    Extension { nibbles: Nibbles, child: SegmentChildMeta },
    Leaf { nibbles: Nibbles, value: Vec<u8> },
}

#[derive(Clone)]
struct SegmentChildMeta {
    slot: u8,
    target_idx: Option<u32>,
    embed: SegmentChildEmbed,
}

#[derive(Clone)]
enum SegmentChildEmbed {
    None,
    Hash(B256),
    Inline(Vec<u8>),
}

impl SegmentChildMeta {
    fn to_child_ref(&self) -> Result<ChildRef> {
        Ok(match &self.embed {
            SegmentChildEmbed::None => {
                return Err(MptDbError::Other("missing child embed".to_string()));
            }
            SegmentChildEmbed::Hash(hash) => ChildRef::Hash(*hash),
            SegmentChildEmbed::Inline(bytes) => ChildRef::Inline(bytes.clone()),
        })
    }
}

fn collect_reachable_nodes(
    nodes: &[MptNode],
    arena_idx: u32,
    arena_to_segment: &mut [Option<u32>],
    reachable: &mut Vec<u32>,
) {
    if arena_to_segment[arena_idx as usize].is_some() {
        return;
    }
    let seg_idx = reachable.len() as u32;
    arena_to_segment[arena_idx as usize] = Some(seg_idx);
    reachable.push(arena_idx);
    match &nodes[arena_idx as usize] {
        MptNode::Leaf(_) => {}
        MptNode::Extension(ext) => {
            if let ChildRef::Arena(child_idx) = ext.child {
                collect_reachable_nodes(nodes, child_idx, arena_to_segment, reachable);
            }
        }
        MptNode::Branch(branch) => {
            for child in branch.children.iter().flatten() {
                if let ChildRef::Arena(child_idx) = child {
                    collect_reachable_nodes(nodes, *child_idx, arena_to_segment, reachable);
                }
            }
        }
    }
}

fn build_node(
    nodes: &[MptNode],
    hash_cache: &[Option<B256>],
    arena_idx: u32,
    arena_to_segment: &[Option<u32>],
) -> Result<BuildNode> {
    let hash = node_hash(nodes, hash_cache, arena_idx)?;
    match &nodes[arena_idx as usize] {
        MptNode::Leaf(leaf) => Ok(BuildNode::Leaf {
            nibbles: leaf.nibbles.iter().collect(),
            value: leaf.value.clone(),
            hash,
        }),
        MptNode::Extension(ext) => Ok(BuildNode::Extension {
            nibbles: ext.nibbles.iter().collect(),
            hash,
            child: build_child(nodes, hash_cache, arena_to_segment, 0, &ext.child)?,
        }),
        MptNode::Branch(branch) => {
            let mut children = Vec::new();
            for (slot, child) in branch.children.iter().enumerate() {
                if let Some(child) = child {
                    children.push(build_child(
                        nodes,
                        hash_cache,
                        arena_to_segment,
                        slot as u8,
                        child,
                    )?);
                }
            }
            Ok(BuildNode::Branch { value: branch.value.clone(), hash, children })
        }
    }
}

fn build_child(
    nodes: &[MptNode],
    hash_cache: &[Option<B256>],
    arena_to_segment: &[Option<u32>],
    slot: u8,
    child: &ChildRef,
) -> Result<BuildChild> {
    Ok(match child {
        ChildRef::Arena(idx) => BuildChild {
            slot,
            target_idx: arena_to_segment.get(*idx as usize).copied().flatten(),
            embed: match child_embed(nodes, hash_cache, *idx)? {
                ChildEmbedOwned::Hash(hash) => ChildEmbedOwned::Hash(hash),
                ChildEmbedOwned::Inline(bytes) => ChildEmbedOwned::Inline(bytes),
            },
        },
        ChildRef::Hash(hash) => {
            BuildChild { slot, target_idx: None, embed: ChildEmbedOwned::Hash(*hash) }
        }
        ChildRef::Inline(bytes) => {
            BuildChild { slot, target_idx: None, embed: ChildEmbedOwned::Inline(bytes.clone()) }
        }
    })
}

fn child_embed(
    nodes: &[MptNode],
    hash_cache: &[Option<B256>],
    idx: u32,
) -> Result<ChildEmbedOwned> {
    if let Some(hash) = hash_cache.get(idx as usize).copied().flatten() {
        return Ok(ChildEmbedOwned::Hash(hash));
    }
    let rlp = encode_node_readonly(nodes, hash_cache, idx)?;
    if rlp.len() < 32 {
        Ok(ChildEmbedOwned::Inline(rlp))
    } else {
        Ok(ChildEmbedOwned::Hash(hash::hash_rlp(&rlp)))
    }
}

fn node_hash(nodes: &[MptNode], hash_cache: &[Option<B256>], idx: u32) -> Result<Option<B256>> {
    if let Some(hash) = hash_cache.get(idx as usize).copied().flatten() {
        return Ok(Some(hash));
    }
    let rlp = encode_node_readonly(nodes, hash_cache, idx)?;
    Ok((rlp.len() >= 32).then_some(hash::hash_rlp(&rlp)))
}

fn encode_node_readonly(
    nodes: &[MptNode],
    hash_cache: &[Option<B256>],
    idx: u32,
) -> Result<Vec<u8>> {
    Ok(match &nodes[idx as usize] {
        MptNode::Leaf(leaf) => encode_leaf(&leaf.nibbles, &leaf.value),
        MptNode::Extension(ext) => {
            let child = encode_child_readonly(nodes, hash_cache, &ext.child)?;
            encode_extension(&ext.nibbles, &child)
        }
        MptNode::Branch(branch) => {
            let mut children: [Option<Vec<u8>>; 16] = std::array::from_fn(|_| None);
            for (i, child) in branch.children.iter().enumerate() {
                if let Some(child) = child {
                    children[i] = Some(encode_child_readonly(nodes, hash_cache, child)?);
                }
            }
            encode_branch(&children, branch.value.as_deref())
        }
    })
}

fn encode_child_readonly(
    nodes: &[MptNode],
    hash_cache: &[Option<B256>],
    child: &ChildRef,
) -> Result<Vec<u8>> {
    Ok(match child {
        ChildRef::Arena(idx) => match child_embed(nodes, hash_cache, *idx)? {
            ChildEmbedOwned::Hash(hash) => hash.to_vec(),
            ChildEmbedOwned::Inline(bytes) => bytes,
        },
        ChildRef::Hash(hash) => hash.to_vec(),
        ChildRef::Inline(bytes) => bytes.clone(),
    })
}

fn encode_segment(build_nodes: &[BuildNode], root: B256, root_idx: u32) -> Vec<u8> {
    let mut child_records = Vec::<[u8; CHILD_RECORD_LEN]>::new();
    let mut blob = Vec::<u8>::new();
    let mut node_records = Vec::<[u8; NODE_RECORD_LEN]>::with_capacity(build_nodes.len());

    for node in build_nodes {
        let mut rec = [0u8; NODE_RECORD_LEN];
        match node {
            BuildNode::Branch { value, hash, children } => {
                rec[0] = TAG_BRANCH;
                if value.is_some() {
                    rec[1] |= 0b01;
                }
                if hash.is_some() {
                    rec[1] |= 0b10;
                }
                let child_offset = child_records.len() as u32;
                rec[2..4].copy_from_slice(&(children.len() as u16).to_le_bytes());
                rec[4..8].copy_from_slice(&child_offset.to_le_bytes());
                let child_bitmap =
                    children.iter().fold(0u16, |acc, child| acc | (1u16 << child.slot));
                rec[14..16].copy_from_slice(&child_bitmap.to_le_bytes());
                if let Some(value) = value {
                    let value_offset = blob.len() as u32;
                    blob.extend_from_slice(value);
                    rec[16..20].copy_from_slice(&value_offset.to_le_bytes());
                    rec[20..24].copy_from_slice(&(value.len() as u32).to_le_bytes());
                }
                if let Some(hash) = hash {
                    rec[24..56].copy_from_slice(hash.as_slice());
                }
                for child in children {
                    child_records.push(encode_child_record(child, &mut blob));
                }
            }
            BuildNode::Extension { nibbles, hash, child } => {
                rec[0] = TAG_EXTENSION;
                if hash.is_some() {
                    rec[1] |= 0b10;
                }
                rec[2..4].copy_from_slice(&1u16.to_le_bytes());
                rec[4..8].copy_from_slice(&(child_records.len() as u32).to_le_bytes());
                let path_offset = blob.len() as u32;
                blob.extend_from_slice(nibbles);
                rec[8..12].copy_from_slice(&path_offset.to_le_bytes());
                rec[12..14].copy_from_slice(&(nibbles.len() as u16).to_le_bytes());
                if let Some(hash) = hash {
                    rec[24..56].copy_from_slice(hash.as_slice());
                }
                child_records.push(encode_child_record(child, &mut blob));
            }
            BuildNode::Leaf { nibbles, value, hash } => {
                rec[0] = TAG_LEAF;
                if hash.is_some() {
                    rec[1] |= 0b10;
                }
                let path_offset = blob.len() as u32;
                blob.extend_from_slice(nibbles);
                rec[8..12].copy_from_slice(&path_offset.to_le_bytes());
                rec[12..14].copy_from_slice(&(nibbles.len() as u16).to_le_bytes());
                let value_offset = blob.len() as u32;
                blob.extend_from_slice(value);
                rec[16..20].copy_from_slice(&value_offset.to_le_bytes());
                rec[20..24].copy_from_slice(&(value.len() as u32).to_le_bytes());
                if let Some(hash) = hash {
                    rec[24..56].copy_from_slice(hash.as_slice());
                }
            }
        }
        node_records.push(rec);
    }

    let blob_offset = (SEGMENT_HEADER_LEN +
        node_records.len() * NODE_RECORD_LEN +
        child_records.len() * CHILD_RECORD_LEN) as u32;

    let mut out = Vec::with_capacity(blob_offset as usize + blob.len());
    out.extend_from_slice(SEGMENT_MAGIC);
    out.extend_from_slice(&SEGMENT_VERSION.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&(node_records.len() as u32).to_le_bytes());
    out.extend_from_slice(&(child_records.len() as u32).to_le_bytes());
    out.extend_from_slice(&root_idx.to_le_bytes());
    out.extend_from_slice(&blob_offset.to_le_bytes());
    out.extend_from_slice(root.as_slice());
    for record in node_records {
        out.extend_from_slice(&record);
    }
    for record in child_records {
        out.extend_from_slice(&record);
    }
    out.extend_from_slice(&blob);
    out
}

fn encode_child_record(child: &BuildChild, blob: &mut Vec<u8>) -> [u8; CHILD_RECORD_LEN] {
    let mut rec = [0u8; CHILD_RECORD_LEN];
    rec[0] = child.slot;
    rec[4..8].copy_from_slice(&child.target_idx.unwrap_or(u32::MAX).to_le_bytes());
    match &child.embed {
        ChildEmbedOwned::Hash(hash) => {
            rec[1] = EMBED_HASH;
            rec[16..48].copy_from_slice(hash.as_slice());
        }
        ChildEmbedOwned::Inline(bytes) => {
            rec[1] = EMBED_INLINE;
            let offset = blob.len() as u32;
            blob.extend_from_slice(bytes);
            rec[8..12].copy_from_slice(&offset.to_le_bytes());
            rec[12..16].copy_from_slice(&(bytes.len() as u32).to_le_bytes());
        }
    }
    rec
}

fn read_u16(bytes: &[u8], off: usize) -> Result<u16> {
    let slice = bytes
        .get(off..off + 2)
        .ok_or_else(|| MptDbError::Other("segment read_u16 out of bounds".to_string()))?;
    Ok(u16::from_le_bytes([slice[0], slice[1]]))
}

fn read_u32(bytes: &[u8], off: usize) -> Result<u32> {
    let slice = bytes
        .get(off..off + 4)
        .ok_or_else(|| MptDbError::Other("segment read_u32 out of bounds".to_string()))?;
    Ok(u32::from_le_bytes([slice[0], slice[1], slice[2], slice[3]]))
}

fn read_b256(bytes: &[u8], off: usize) -> Result<B256> {
    let slice = bytes
        .get(off..off + 32)
        .ok_or_else(|| MptDbError::Other("segment read_b256 out of bounds".to_string()))?;
    Ok(B256::from_slice(slice))
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::keccak256;
    use std::{fs::File, io::Write};
    use tempfile::NamedTempFile;

    #[test]
    fn t4_8_segment_roundtrip_all_paths() {
        let mut tree = MptTree::new();
        let mut keys = Vec::new();
        for i in 0u8..16 {
            let key = Nibbles::unpack(keccak256(&[i]));
            keys.push(key.clone());
            tree.insert(&key, vec![i; 32]);
        }
        let root = tree.root_hash();
        let segment = StorageTrieSegment::from_tree(&tree, root).unwrap();
        let reader = StorageTrieSegmentReader::open(segment.as_bytes(), root).unwrap();
        let mut loaded = reader.materialize_touched_paths(&keys).unwrap();
        assert_eq!(loaded.root_hash(), root);
    }

    #[test]
    fn t4_9_segment_touched_path_update_matches_full_tree() {
        let mut full = MptTree::new();
        let mut touched = Vec::new();
        for i in 0u8..20 {
            let key = Nibbles::unpack(keccak256(&[i]));
            if i < 4 {
                touched.push(key.clone());
            }
            full.insert(&key, vec![i; 32]);
        }
        let root = full.root_hash();
        let segment = StorageTrieSegment::from_tree(&full, root).unwrap();
        let new_key = Nibbles::unpack(keccak256(b"new-key"));
        let touched_all = [touched[0].clone(), new_key.clone()];
        let reader = StorageTrieSegmentReader::open(segment.as_bytes(), root).unwrap();
        let mut partial = reader.materialize_touched_paths(&touched_all).unwrap();
        partial.insert(&touched_all[0], b"updated".to_vec());
        partial.insert(&touched_all[1], b"new".to_vec());

        full.insert(&touched_all[0], b"updated".to_vec());
        full.insert(&touched_all[1], b"new".to_vec());

        assert_eq!(partial.root_hash(), full.root_hash());
    }

    #[test]
    fn t4_10_segment_view_node_is_borrowed() {
        let mut tree = MptTree::new();
        let key = Nibbles::unpack(keccak256(b"view-node"));
        tree.insert(&key, b"value".to_vec());
        let root = tree.root_hash();
        let segment = StorageTrieSegment::from_tree(&tree, root).unwrap();
        let reader = StorageTrieSegmentReader::open(segment.as_bytes(), root).unwrap();
        let view = reader.view_node(reader.root_idx).unwrap();
        match view.kind {
            SegmentNodeKind::Leaf { nibbles, value } => {
                assert!(!nibbles.is_empty());
                assert_eq!(value, b"value");
            }
            SegmentNodeKind::Extension { child, .. } => {
                assert!(child.target_idx.is_some());
            }
            SegmentNodeKind::Branch { child_bitmap, .. } => {
                assert_ne!(child_bitmap, 0);
            }
        }
    }

    #[test]
    fn t4_11_segment_open_shared_page_keeps_runtime_ref() {
        let mut tree = MptTree::new();
        let key = Nibbles::unpack(keccak256(b"shared-page"));
        tree.insert(&key, b"value".to_vec());
        let root = tree.root_hash();
        let segment = StorageTrieSegment::from_tree(&tree, root).unwrap();

        let mut tmp = NamedTempFile::new().unwrap();
        tmp.write_all(segment.as_bytes()).unwrap();
        tmp.flush().unwrap();
        let file = File::open(tmp.path()).unwrap();
        let mmap = unsafe { Mmap::map(&file).unwrap() };
        let mapped = Arc::new(MappedSegmentPage::new(Arc::new(mmap), 0, segment.as_bytes().len()));
        let lease = Arc::new(SegmentPageLease::new(mapped, root, segment.root_record_off()));
        let reader =
            StorageTrieSegmentReader::open_shared_page(&lease, root, segment.root_record_off())
                .unwrap();
        let root_ref = reader.root_ref().unwrap();
        assert_eq!(root_ref.page_lease().root(), root);
    }
}
