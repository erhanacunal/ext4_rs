//! Linear directories: blocks of entries, each block ending in the 12-byte checksum entry
//! `metadata_csum` adds. Directories are metadata: their blocks go through the cache and the
//! journal.

use crate::bytes::*;
use crate::crc32c::crc32c;
use crate::extent::Mapping;
use crate::fs::{Ext4, Ino};
use crate::inode::Inode;
use crate::{BLOCK_SIZE, BlockDevice, Error, Result};

pub const FT_REG: u8 = 1;
pub const FT_DIR: u8 = 2;
/// Where the checksum entry starts.
const TAIL_AT: usize = BLOCK_SIZE - 12;

/// Bytes an entry with a name of `len` bytes needs.
fn needed(len: usize) -> usize {
    (8 + len + 3) & !3
}

fn put_entry(b: &mut [u8], at: usize, ino: Ino, rec_len: usize, name: &[u8], ft: u8) {
    set_le32(b, at, ino);
    set_le16(b, at + 4, rec_len as u16);
    b[at + 6] = name.len() as u8;
    b[at + 7] = ft;
    b[at + 8..at + 8 + name.len()].copy_from_slice(name);
}

/// The checksum entry and its checksum.
fn seal(b: &mut [u8], seed: u32) {
    b[TAIL_AT..].fill(0);
    set_le16(b, TAIL_AT + 4, 12);
    b[TAIL_AT + 7] = 0xDE;
    let c = crc32c(seed, &b[..TAIL_AT]);
    set_le32(b, TAIL_AT + 8, c);
}

/// A new directory's first block: `.` and `..`.
pub fn init_block(b: &mut [u8], ino: Ino, parent: Ino, seed: u32) {
    put_entry(b, 0, ino, 12, b".", FT_DIR);
    put_entry(b, 12, parent, TAIL_AT - 12, b"..", FT_DIR);
    seal(b, seed);
}

/// One entry: offset, inode, record length, name length, file type.
#[derive(Clone, Copy)]
struct Raw {
    at: usize,
    ino: Ino,
    rec_len: usize,
    name_len: usize,
    ft: u8,
}

/// The entries of a directory block, checked.
fn entries(b: &[u8], seed: u32) -> Result<alloc::vec::Vec<Raw>> {
    if le32(b, TAIL_AT) != 0 || le16(b, TAIL_AT + 4) != 12 || b[TAIL_AT + 7] != 0xDE {
        return Err(Error::Corrupt("directory block without a checksum entry"));
    }
    if le32(b, TAIL_AT + 8) != crc32c(seed, &b[..TAIL_AT]) {
        return Err(Error::Corrupt("directory block checksum"));
    }
    let mut out = alloc::vec::Vec::new();
    let mut at = 0;
    while at < TAIL_AT {
        let rec_len = le16(b, at + 4) as usize;
        let name_len = b[at + 6] as usize;
        if rec_len < 12 || rec_len % 4 != 0 || at + rec_len > TAIL_AT || needed(name_len) > rec_len {
            return Err(Error::Corrupt("directory entry"));
        }
        out.push(Raw { at, ino: le32(b, at), rec_len, name_len, ft: b[at + 7] });
        at += rec_len;
    }
    Ok(out)
}

impl<D: BlockDevice> Ext4<D> {
    fn dir_seed(&self, ino: Ino, inode: &Inode) -> u32 {
        Inode::seed(self.seed, ino, inode.generation())
    }

    fn dir_block_count(inode: &Inode) -> Result<u32> {
        let size = inode.size();
        if size % BLOCK_SIZE as u64 != 0 || size / (BLOCK_SIZE as u64) > u32::MAX as u64 {
            return Err(Error::Corrupt("directory size"));
        }
        Ok((size / BLOCK_SIZE as u64) as u32)
    }

    fn dir_block(&mut self, ino: Ino, inode: &Inode, i: u32) -> Result<u64> {
        match self.extent_map(ino, inode, i)? {
            Mapping::Mapped(p, _) => Ok(p),
            Mapping::Hole(_) => Err(Error::Corrupt("hole in a directory")),
        }
    }

    /// Calls `f(name, inode, file type)` for each entry until it returns `false`.
    pub(crate) fn dir_for_each(&mut self, ino: Ino, inode: &Inode, mut f: impl FnMut(&[u8], Ino, u8) -> bool) -> Result<()> {
        let seed = self.dir_seed(ino, inode);
        for i in 0..Self::dir_block_count(inode)? {
            let p = self.dir_block(ino, inode, i)?;
            let b = self.cache.get(&mut self.dev, p)?;
            for e in entries(b, seed)? {
                if e.ino != 0 && !f(&b[e.at + 8..e.at + 8 + e.name_len], e.ino, e.ft) {
                    return Ok(());
                }
            }
        }
        Ok(())
    }

    /// Finds `name`: its block, entry offset, inode and file type.
    fn dir_find(&mut self, ino: Ino, inode: &Inode, name: &[u8]) -> Result<Option<(u64, usize, Ino, u8)>> {
        let seed = self.dir_seed(ino, inode);
        for i in 0..Self::dir_block_count(inode)? {
            let p = self.dir_block(ino, inode, i)?;
            let b = self.cache.get(&mut self.dev, p)?;
            for e in entries(b, seed)? {
                if e.ino != 0 && &b[e.at + 8..e.at + 8 + e.name_len] == name {
                    return Ok(Some((p, e.at, e.ino, e.ft)));
                }
            }
        }
        Ok(None)
    }

    pub(crate) fn dir_lookup(&mut self, ino: Ino, inode: &Inode, name: &[u8]) -> Result<Option<(Ino, u8)>> {
        Ok(self.dir_find(ino, inode, name)?.map(|(_, _, i, ft)| (i, ft)))
    }

    pub(crate) fn dir_is_empty(&mut self, ino: Ino, inode: &Inode) -> Result<bool> {
        let mut empty = true;
        self.dir_for_each(ino, inode, |name, _, _| {
            empty = name == b"." || name == b"..";
            empty
        })?;
        Ok(empty)
    }

    /// Adds `name` → `child` (the name must not exist). May grow the directory by a block;
    /// the caller writes the directory's inode.
    pub(crate) fn dir_add(&mut self, ino: Ino, inode: &mut Inode, name: &[u8], child: Ino, ft: u8) -> Result<()> {
        let seed = self.dir_seed(ino, inode);
        let need = needed(name.len());
        let blocks = Self::dir_block_count(inode)?;
        for i in 0..blocks {
            let p = self.dir_block(ino, inode, i)?;
            let slot = {
                let b = self.cache.get(&mut self.dev, p)?;
                entries(b, seed)?.into_iter().find_map(|e| {
                    let used = if e.ino == 0 { 0 } else { needed(e.name_len) };
                    (e.rec_len - used >= need).then_some((e, used))
                })
            };
            if let Some((e, used)) = slot {
                let b = self.cache.get_mut(&mut self.dev, p)?;
                if used == 0 {
                    put_entry(b, e.at, child, e.rec_len, name, ft);
                } else {
                    set_le16(b, e.at + 4, used as u16);
                    put_entry(b, e.at + used, child, e.rec_len - used, name, ft);
                }
                seal(b, seed);
                return Ok(());
            }
        }
        // A new block at the end.
        let goal = match blocks {
            0 => 0,
            n => self.dir_block(ino, inode, n - 1)? + 1,
        };
        let (p, _) = self.alloc_blocks_for(goal, 1, true)?;
        let b = self.cache.zeroed(p);
        put_entry(b, 0, child, TAIL_AT, name, ft);
        seal(b, seed);
        self.extent_insert(ino, inode, blocks, p, 1)?;
        inode.add_blocks(1);
        inode.set_size((blocks as u64 + 1) * BLOCK_SIZE as u64);
        Ok(())
    }

    /// Removes `name` (blocks are not given back; the directory keeps its size).
    pub(crate) fn dir_remove(&mut self, ino: Ino, inode: &Inode, name: &[u8]) -> Result<()> {
        let seed = self.dir_seed(ino, inode);
        let (p, at, _, _) = self.dir_find(ino, inode, name)?.ok_or(Error::NotFound)?;
        let b = self.cache.get_mut(&mut self.dev, p)?;
        let list = entries(b, seed)?;
        let i = list.iter().position(|e| e.at == at).unwrap();
        if i == 0 {
            // First in its block: emptied in place.
            set_le32(b, at, 0);
        } else {
            let prev = list[i - 1];
            set_le16(b, prev.at + 4, (prev.rec_len + list[i].rec_len) as u16);
        }
        seal(b, seed);
        Ok(())
    }

    /// Points the existing entry `name` at `child`.
    pub(crate) fn dir_set_entry(&mut self, ino: Ino, inode: &Inode, name: &[u8], child: Ino, ft: u8) -> Result<()> {
        let seed = self.dir_seed(ino, inode);
        let (p, at, _, _) = self.dir_find(ino, inode, name)?.ok_or(Error::NotFound)?;
        let b = self.cache.get_mut(&mut self.dev, p)?;
        set_le32(b, at, child);
        b[at + 7] = ft;
        seal(b, seed);
        Ok(())
    }
}
