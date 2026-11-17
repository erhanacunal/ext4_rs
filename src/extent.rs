//! The extent tree: an inode's logical blocks → physical blocks.
//!
//! The root is the inode's `i_block` (4 entries); other nodes are blocks (340 entries and a
//! checksum tail). Index entries point at the node below; leaves hold extents of up to 32768
//! blocks. Unwritten extents are never made, and refused when met.

use alloc::vec::Vec;

use crate::bytes::*;
use crate::crc32c::crc32c;
use crate::fs::{Ext4, Ino};
use crate::inode::Inode;
use crate::{BLOCK_SIZE, BlockDevice, Error, Result};

const MAGIC: u16 = 0xF30A;
const HEADER: usize = 12;
const ENTRY: usize = 12;
const ROOT_MAX: u16 = 4;
const BLOCK_MAX: u16 = ((BLOCK_SIZE - HEADER - 4) / ENTRY) as u16;
/// Where the checksum is in a node block.
const TAIL: usize = HEADER + BLOCK_MAX as usize * ENTRY;
pub const MAX_LEN: u32 = 32768;
/// Deepest tree accepted (Linux makes at most 5 levels).
const MAX_DEPTH: u16 = 5;

/// What a logical block maps to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mapping {
    /// Physical block, and how many blocks from here are contiguous in this extent.
    Mapped(u64, u32),
    /// No block, for this many blocks (up to the next extent).
    Hole(u32),
}

#[derive(Clone, Copy, Debug)]
struct Entry {
    /// First logical block.
    lblk: u32,
    /// Blocks (leaves only).
    len: u32,
    /// First physical block (leaf), or the child node's block (index).
    ptr: u64,
}

struct Node {
    depth: u16,
    max: u16,
    entries: Vec<Entry>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Loc {
    Root,
    Block(u64),
}

/// An empty leaf root in a new inode's `i_block`.
pub fn init_root(i_block: &mut [u8]) {
    i_block.fill(0);
    set_le16(i_block, 0, MAGIC);
    set_le16(i_block, 2, 0);
    set_le16(i_block, 4, ROOT_MAX);
    set_le16(i_block, 6, 0);
}

fn parse(b: &[u8]) -> Result<Node> {
    if le16(b, 0) != MAGIC {
        return Err(Error::Corrupt("extent header magic"));
    }
    let count = le16(b, 2);
    let max = le16(b, 4);
    let depth = le16(b, 6);
    if count > max || HEADER + max as usize * ENTRY > b.len() || depth > MAX_DEPTH {
        return Err(Error::Corrupt("extent header"));
    }
    let mut entries = Vec::with_capacity(count as usize + 1);
    for i in 0..count as usize {
        let e = &b[HEADER + i * ENTRY..];
        let lblk = le32(e, 0);
        let entry = if depth == 0 {
            let len = le16(e, 4) as u32;
            if len > MAX_LEN {
                return Err(Error::Unsupported("unwritten extent"));
            }
            if len == 0 {
                return Err(Error::Corrupt("empty extent"));
            }
            Entry { lblk, len, ptr: (le16(e, 6) as u64) << 32 | le32(e, 8) as u64 }
        } else {
            Entry { lblk, len: 0, ptr: (le16(e, 8) as u64) << 32 | le32(e, 4) as u64 }
        };
        if let Some(prev) = entries.last() {
            let prev: &Entry = prev;
            if entry.lblk <= prev.lblk || (depth == 0 && prev.lblk as u64 + prev.len as u64 > entry.lblk as u64) {
                return Err(Error::Corrupt("extents out of order"));
            }
        }
        entries.push(entry);
    }
    Ok(Node { depth, max, entries })
}

fn serialize(node: &Node, b: &mut [u8]) {
    let area = HEADER + node.max as usize * ENTRY;
    b[..area].fill(0);
    set_le16(b, 0, MAGIC);
    set_le16(b, 2, node.entries.len() as u16);
    set_le16(b, 4, node.max);
    set_le16(b, 6, node.depth);
    for (i, e) in node.entries.iter().enumerate() {
        let at = HEADER + i * ENTRY;
        set_le32(b, at, e.lblk);
        if node.depth == 0 {
            set_le16(b, at + 4, e.len as u16);
            set_le16(b, at + 6, (e.ptr >> 32) as u16);
            set_le32(b, at + 8, e.ptr as u32);
        } else {
            set_le32(b, at + 4, e.ptr as u32);
            set_le16(b, at + 8, (e.ptr >> 32) as u16);
        }
    }
}

/// The child of an index node that holds `lblk`: the last entry starting at or before it
/// (the first when `lblk` is before all).
fn child_index(node: &Node, lblk: u32) -> usize {
    node.entries.iter().rposition(|e| e.lblk <= lblk).unwrap_or(0)
}

impl<D: BlockDevice> Ext4<D> {
    fn load_node(&mut self, inode: &Inode, seed: u32, at: Loc) -> Result<Node> {
        match at {
            Loc::Root => parse(inode.i_block()),
            Loc::Block(nr) => {
                if nr >= self.geo.blocks {
                    return Err(Error::Corrupt("extent node past the end"));
                }
                let b = self.cache.get(&mut self.dev, nr)?;
                if le32(b, TAIL) != crc32c(seed, &b[..TAIL]) {
                    return Err(Error::Corrupt("extent block checksum"));
                }
                let node = parse(b)?;
                if node.max != BLOCK_MAX {
                    return Err(Error::Corrupt("extent block size"));
                }
                Ok(node)
            }
        }
    }

    fn store_node(&mut self, inode: &mut Inode, seed: u32, at: Loc, node: &Node) -> Result<()> {
        match at {
            Loc::Root => serialize(node, inode.i_block_mut()),
            Loc::Block(nr) => {
                let b = self.cache.get_mut(&mut self.dev, nr)?;
                serialize(node, b);
                let c = crc32c(seed, &b[..TAIL]);
                set_le32(b, TAIL, c);
            }
        }
        Ok(())
    }

    /// A new tree node block, near `goal`.
    fn alloc_node(&mut self, inode: &mut Inode, goal: u64) -> Result<u64> {
        let (nr, _) = self.alloc_blocks_for(goal, 1, true)?;
        self.cache.zeroed(nr);
        inode.add_blocks(1);
        Ok(nr)
    }

    /// What logical block `lblk` of the inode maps to.
    pub(crate) fn extent_map(&mut self, ino: Ino, inode: &Inode, lblk: u32) -> Result<Mapping> {
        let seed = Inode::seed(self.seed, ino, inode.generation());
        let mut node = self.load_node(inode, seed, Loc::Root)?;
        // The first logical block past this subtree.
        let mut bound = u32::MAX;
        while node.depth > 0 {
            if node.entries.is_empty() {
                return Err(Error::Corrupt("empty extent index"));
            }
            if lblk < node.entries[0].lblk {
                return Ok(Mapping::Hole(node.entries[0].lblk - lblk));
            }
            let i = child_index(&node, lblk);
            if let Some(next) = node.entries.get(i + 1) {
                bound = bound.min(next.lblk);
            }
            let depth = node.depth;
            node = self.load_node(inode, seed, Loc::Block(node.entries[i].ptr))?;
            if node.depth != depth - 1 {
                return Err(Error::Corrupt("extent tree depth"));
            }
        }
        for e in &node.entries {
            if lblk < e.lblk {
                return Ok(Mapping::Hole(e.lblk.min(bound) - lblk));
            }
            if lblk - e.lblk < e.len {
                let skip = lblk - e.lblk;
                return Ok(Mapping::Mapped(e.ptr + skip as u64, e.len - skip));
            }
        }
        Ok(Mapping::Hole(bound - lblk))
    }

    /// Maps logical blocks `lblk..lblk + len` (unmapped now) to physical `pblk..`. Tree blocks
    /// it allocates are added to the inode's block count; the caller writes the inode.
    pub(crate) fn extent_insert(&mut self, ino: Ino, inode: &mut Inode, lblk: u32, pblk: u64, len: u32) -> Result<()> {
        let seed = Inode::seed(self.seed, ino, inode.generation());
        let mut l = lblk;
        let mut p = pblk;
        let mut left = len;
        while left > 0 {
            let n = left.min(MAX_LEN);
            let split = self.insert_rec(inode, seed, Loc::Root, Entry { lblk: l, len: n, ptr: p })?;
            debug_assert!(split.is_none());
            l += n;
            p += n as u64;
            left -= n;
        }
        Ok(())
    }

    /// Inserts `ext` under `at`; returns the index entry of a new right sibling when `at` (not
    /// the root) had to split.
    fn insert_rec(&mut self, inode: &mut Inode, seed: u32, at: Loc, ext: Entry) -> Result<Option<Entry>> {
        let mut node = self.load_node(inode, seed, at)?;
        let pos;
        if node.depth == 0 {
            pos = node.entries.iter().position(|e| e.lblk > ext.lblk).unwrap_or(node.entries.len());
            if pos > 0 {
                let prev = node.entries[pos - 1];
                if prev.lblk + prev.len > ext.lblk {
                    return Err(Error::Corrupt("inserting a mapped block"));
                }
            }
            if let Some(next) = node.entries.get(pos) {
                if ext.lblk + ext.len > next.lblk {
                    return Err(Error::Corrupt("inserting over a mapped block"));
                }
            }
            // Merge with the extent before or after when contiguous both ways.
            let merge_prev = pos > 0 && {
                let prev = node.entries[pos - 1];
                prev.lblk + prev.len == ext.lblk && prev.ptr + prev.len as u64 == ext.ptr && prev.len + ext.len <= MAX_LEN
            };
            let merge_next = !merge_prev
                && node.entries.get(pos).is_some_and(|next| {
                    ext.lblk + ext.len == next.lblk && ext.ptr + ext.len as u64 == next.ptr && next.len + ext.len <= MAX_LEN
                });
            if merge_prev {
                node.entries[pos - 1].len += ext.len;
            } else if merge_next {
                let next = &mut node.entries[pos];
                next.lblk = ext.lblk;
                next.ptr = ext.ptr;
                next.len += ext.len;
            } else {
                node.entries.insert(pos, ext);
            }
        } else {
            if node.entries.is_empty() {
                return Err(Error::Corrupt("empty extent index"));
            }
            let i = child_index(&node, ext.lblk);
            let child = node.entries[i].ptr;
            let split = self.insert_rec(inode, seed, Loc::Block(child), ext)?;
            let key_moved = ext.lblk < node.entries[i].lblk;
            if key_moved {
                node.entries[i].lblk = ext.lblk;
            }
            pos = i + 1;
            match split {
                Some(s) => node.entries.insert(pos, s),
                None => {
                    if key_moved {
                        self.store_node(inode, seed, at, &node)?;
                    }
                    return Ok(None);
                }
            }
        }

        if node.entries.len() <= node.max as usize {
            self.store_node(inode, seed, at, &node)?;
            return Ok(None);
        }
        if node.depth + 1 > MAX_DEPTH {
            return Err(Error::TooLarge);
        }
        let goal = node.entries.last().map(|e| e.ptr).unwrap_or(0);
        match at {
            Loc::Root => {
                // The root moves down into a new block; the root becomes an index over it.
                let nr = self.alloc_node(inode, goal)?;
                let child = Node { depth: node.depth, max: BLOCK_MAX, entries: node.entries };
                self.store_node(inode, seed, Loc::Block(nr), &child)?;
                let root = Node {
                    depth: child.depth + 1,
                    max: ROOT_MAX,
                    entries: alloc::vec![Entry { lblk: child.entries[0].lblk, len: 0, ptr: nr }],
                };
                self.store_node(inode, seed, Loc::Root, &root)?;
                Ok(None)
            }
            Loc::Block(_) => {
                // Appending (the usual case) moves only the new entry right; otherwise halve.
                let k = if pos == node.entries.len() - 1 { pos } else { node.entries.len() / 2 };
                let right_entries = node.entries.split_off(k);
                let nr = self.alloc_node(inode, goal)?;
                let right = Node { depth: node.depth, max: BLOCK_MAX, entries: right_entries };
                self.store_node(inode, seed, Loc::Block(nr), &right)?;
                self.store_node(inode, seed, at, &node)?;
                Ok(Some(Entry { lblk: right.entries[0].lblk, len: 0, ptr: nr }))
            }
        }
    }

    /// Unmaps and frees every block from logical block `from` on (tree blocks that empty are
    /// freed too). The caller writes the inode.
    pub(crate) fn extent_truncate(&mut self, ino: Ino, inode: &mut Inode, from: u32) -> Result<()> {
        let seed = Inode::seed(self.seed, ino, inode.generation());
        self.truncate_rec(inode, seed, Loc::Root, from)?;
        Ok(())
    }

    /// Returns how many entries `at` keeps.
    fn truncate_rec(&mut self, inode: &mut Inode, seed: u32, at: Loc, from: u32) -> Result<usize> {
        let mut node = self.load_node(inode, seed, at)?;
        let before = node.entries.len();
        let mut changed = false;
        if node.depth == 0 {
            while let Some(&e) = node.entries.last() {
                if e.lblk >= from {
                    self.free_blocks(e.ptr, e.len)?;
                    inode.add_blocks(-(e.len as i64));
                    node.entries.pop();
                    changed = true;
                } else {
                    if e.lblk + e.len > from {
                        let keep = from - e.lblk;
                        self.free_blocks(e.ptr + keep as u64, e.len - keep)?;
                        inode.add_blocks(-((e.len - keep) as i64));
                        node.entries.last_mut().unwrap().len = keep;
                        changed = true;
                    }
                    break;
                }
            }
        } else {
            let mut i = node.entries.len();
            while i > 0 {
                i -= 1;
                let e = node.entries[i];
                let end = node.entries.get(i + 1).map_or(u32::MAX, |n| n.lblk);
                if end <= from && i + 1 < node.entries.len() {
                    break;
                }
                let kept = self.truncate_rec(inode, seed, Loc::Block(e.ptr), from)?;
                if kept == 0 {
                    self.free_blocks(e.ptr, 1)?;
                    inode.add_blocks(-1);
                    node.entries.remove(i);
                    changed = true;
                }
                if e.lblk < from {
                    break;
                }
            }
            if node.entries.is_empty() && at == Loc::Root {
                // Nothing left: back to an empty leaf.
                node.depth = 0;
                changed = true;
            }
        }
        if changed || before != node.entries.len() {
            self.store_node(inode, seed, at, &node)?;
        }
        Ok(node.entries.len())
    }
}

/// Every extent of an inode whose tree is only its root (the journal's), without a mounted
/// file system: (logical, physical, length).
pub(crate) fn root_extents(inode: &Inode) -> Result<Vec<(u32, u64, u32)>> {
    let node = parse(inode.i_block())?;
    if node.depth != 0 {
        return Err(Error::Unsupported("journal extent tree deeper than its root"));
    }
    Ok(node.entries.iter().map(|e| (e.lblk, e.ptr, e.len)).collect())
}

/// Sets the root of a new inode to `list` (at most 4 extents of at most [`MAX_LEN`] blocks,
/// in order), for [`crate::format`].
pub(crate) fn set_root_extents(inode: &mut Inode, list: &[(u32, u64, u32)]) {
    let entries = list.iter().map(|&(lblk, ptr, len)| Entry { lblk, len, ptr }).collect();
    serialize(&Node { depth: 0, max: ROOT_MAX, entries }, inode.i_block_mut());
}
