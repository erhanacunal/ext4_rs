//! Block and inode allocation: the bitmaps (in the cache), the group descriptors and the
//! superblock counters (in memory, written at commit).

use crate::fs::{Ext4, Ino};
use crate::layout::*;
use crate::{BlockDevice, Error, Result};

/// Blocks only metadata (directory and extent tree blocks) may take, so a full device still
/// lets an operation that started finish.
pub const META_RESERVE: u64 = 64;

fn bit(map: &[u8], i: u32) -> bool {
    map[(i / 8) as usize] & (1 << (i % 8)) != 0
}

/// Whether `block` was freed in the running transaction.
fn freed_now(freed: &alloc::collections::BTreeMap<u64, u32>, block: u64) -> bool {
    match freed.range(..=block).next_back() {
        Some((&start, &len)) => block < start + len as u64,
        None => false,
    }
}

fn set_bit(map: &mut [u8], i: u32, on: bool) {
    let b = &mut map[(i / 8) as usize];
    if on { *b |= 1 << (i % 8) } else { *b &= !(1 << (i % 8)) }
}

impl<D: BlockDevice> Ext4<D> {

    /// Allocates up to `want` contiguous blocks (at least one), the first at or after `goal`
    /// if possible. `meta` blocks may use [`META_RESERVE`].
    pub(crate) fn alloc_blocks_for(&mut self, goal: u64, want: u32, meta: bool) -> Result<(u64, u32)> {
        let reserve = if meta { 0 } else { META_RESERVE };
        if self.sb.free_blocks() <= reserve {
            return Err(Error::NoSpace);
        }
        let want = (want as u64).min(self.sb.free_blocks() - reserve).max(1) as u32;
        let goal = if goal >= self.geo.blocks { 0 } else { goal };
        let start_group = (goal / BLOCKS_PER_GROUP as u64) as u32;
        let groups = self.geo.groups;
        for k in 0..=groups {
            let g = (start_group + k) % groups;
            if self.groups[g as usize].free_blocks() == 0 {
                continue;
            }
            let base = self.geo.group_start(g);
            let count = self.geo.group_blocks(g);
            // The start group twice: from the goal, and last from its start.
            let (from, to) = match k {
                0 => ((goal - base) as u32, count),
                _ if k == groups => (0, ((goal - base) as u32).min(count)),
                _ => (0, count),
            };
            if from >= to {
                continue;
            }
            let bitmap_block = self.geo.block_bitmap(g);
            let found = {
                let freed = &self.freed;
                let map = self.cache.get(&mut self.dev, bitmap_block)?;
                let mut i = from;
                let mut hit = None;
                while i < to {
                    if i % 8 == 0 && map[(i / 8) as usize] == 0xFF {
                        i += 8;
                        continue;
                    }
                    if !bit(map, i) && !freed_now(freed, base + i as u64) {
                        let mut n = 1;
                        while n < want && i + n < count && !bit(map, i + n) && !freed_now(freed, base + (i + n) as u64) {
                            n += 1;
                        }
                        hit = Some((i, n));
                        break;
                    }
                    i += 1;
                }
                hit
            };
            if let Some((i, n)) = found {
                let map = self.cache.get_mut(&mut self.dev, bitmap_block)?;
                for b in i..i + n {
                    set_bit(map, b, true);
                }
                let desc = &mut self.groups[g as usize];
                let free = desc.free_blocks() - n;
                desc.set_u16(gd::FREE_BLOCKS, free as u16);
                self.dirty_groups.insert(g);
                let free = self.sb.free_blocks() - n as u64;
                self.sb.set_free_blocks(free);
                self.sb_dirty = true;
                return Ok((base + i as u64, n));
            }
        }
        Err(Error::NoSpace)
    }

    /// Data blocks: [`Self::alloc_blocks_for`] outside the reserve.
    pub(crate) fn alloc_blocks(&mut self, goal: u64, want: u32) -> Result<(u64, u32)> {
        self.alloc_blocks_for(goal, want, false)
    }

    /// Frees `start..start + len` (they must be in use) and forgets them in the cache.
    pub(crate) fn free_blocks(&mut self, start: u64, len: u32) -> Result<()> {
        let mut b = start;
        let end = start + len as u64;
        while b < end {
            if b >= self.geo.blocks {
                return Err(Error::Corrupt("freeing a block past the end"));
            }
            let g = (b / BLOCKS_PER_GROUP as u64) as u32;
            let base = self.geo.group_start(g);
            let stop = end.min(base + self.geo.group_blocks(g) as u64);
            let n = (stop - b) as u32;
            if b < base + self.geo.overhead(g) as u64 {
                return Err(Error::Corrupt("freeing group metadata"));
            }
            let map = self.cache.get_mut(&mut self.dev, self.geo.block_bitmap(g))?;
            for i in (b - base) as u32..(stop - base) as u32 {
                if !bit(map, i) {
                    return Err(Error::Corrupt("freeing a free block"));
                }
                set_bit(map, i, false);
            }
            let desc = &mut self.groups[g as usize];
            let free = desc.free_blocks() + n;
            desc.set_u16(gd::FREE_BLOCKS, free as u16);
            self.dirty_groups.insert(g);
            let free = self.sb.free_blocks() + n as u64;
            self.sb.set_free_blocks(free);
            self.sb_dirty = true;
            b = stop;
        }
        self.cache.forget_range(start, len as u64);
        // Merge with a run freed just before, else record.
        match self.freed.range(..start).next_back().map(|(&s, &l)| (s, l)) {
            Some((s, l)) if s + l as u64 == start => {
                self.freed.insert(s, l + len);
            }
            _ => {
                self.freed.insert(start, len);
            }
        }
        Ok(())
    }

    /// Allocates an inode, preferring group `goal`.
    pub(crate) fn alloc_inode(&mut self, dir: bool, goal: u32) -> Result<Ino> {
        if self.sb.free_inodes() == 0 {
            return Err(Error::NoSpace);
        }
        let ipg = self.geo.inodes_per_group;
        let groups = self.geo.groups;
        for k in 0..groups {
            let g = (goal + k) % groups;
            if self.groups[g as usize].free_inodes() == 0 {
                continue;
            }
            let bitmap_block = self.geo.block_bitmap(g) + 1;
            let found = {
                let map = self.cache.get(&mut self.dev, bitmap_block)?;
                (0..ipg).find(|&i| !bit(map, i))
            };
            let Some(i) = found else {
                return Err(Error::Corrupt("inode bitmap full, descriptor not"));
            };
            let map = self.cache.get_mut(&mut self.dev, bitmap_block)?;
            set_bit(map, i, true);
            let desc = &mut self.groups[g as usize];
            let free = desc.free_inodes() - 1;
            desc.set_u16(gd::FREE_INODES, free as u16);
            if dir {
                let dirs = desc.u16(gd::USED_DIRS) + 1;
                desc.set_u16(gd::USED_DIRS, dirs);
            }
            // The unused tail of the inode table shrinks past the new inode.
            let unused = desc.u16(gd::ITABLE_UNUSED) as u32;
            if i >= ipg - unused {
                desc.set_u16(gd::ITABLE_UNUSED, (ipg - i - 1) as u16);
            }
            self.dirty_groups.insert(g);
            let free = self.sb.free_inodes() - 1;
            self.sb.set_free_inodes(free);
            self.sb_dirty = true;
            return Ok(g * ipg + i + 1);
        }
        Err(Error::NoSpace)
    }

    pub(crate) fn free_inode(&mut self, ino: Ino, dir: bool) -> Result<()> {
        let g = self.group_of_ino(ino);
        let i = (ino - 1) % self.geo.inodes_per_group;
        let map = self.cache.get_mut(&mut self.dev, self.geo.block_bitmap(g) + 1)?;
        if !bit(map, i) {
            return Err(Error::Corrupt("freeing a free inode"));
        }
        set_bit(map, i, false);
        let desc = &mut self.groups[g as usize];
        let free = desc.free_inodes() + 1;
        desc.set_u16(gd::FREE_INODES, free as u16);
        if dir {
            let dirs = desc.u16(gd::USED_DIRS) - 1;
            desc.set_u16(gd::USED_DIRS, dirs);
        }
        self.dirty_groups.insert(g);
        let free = self.sb.free_inodes() + 1;
        self.sb.set_free_inodes(free);
        self.sb_dirty = true;
        Ok(())
    }

    /// Puts the superblock and the changed descriptors (with their bitmaps' checksums) in
    /// the cache, so the transaction carries them.
    pub(crate) fn stage_counters(&mut self) -> Result<()> {
        let ipg = self.geo.inodes_per_group;
        let groups: alloc::vec::Vec<u32> = self.dirty_groups.iter().copied().collect();
        for &g in &groups {
            let bb = self.geo.block_bitmap(g);
            let bcsum = bitmap_checksum(self.seed, self.cache.get(&mut self.dev, bb)?, BLOCKS_PER_GROUP);
            let icsum = bitmap_checksum(self.seed, self.cache.get(&mut self.dev, bb + 1)?, ipg);
            let desc = &mut self.groups[g as usize];
            desc.set_u16(gd::BLOCK_BITMAP_CSUM, bcsum);
            desc.set_u16(gd::INODE_BITMAP_CSUM, icsum);
            desc.set_checksum(self.seed, g);
            let raw = desc.raw;
            let block = self.cache.get_mut(&mut self.dev, 1 + (g / DESCS_PER_BLOCK) as u64)?;
            let at = (g % DESCS_PER_BLOCK) as usize * DESC_SIZE;
            block[at..at + DESC_SIZE].copy_from_slice(&raw);
        }
        self.dirty_groups.clear();
        if self.sb_dirty {
            let now = self.now();
            self.sb.set_u32(sb::WTIME, now);
            self.sb.set_checksum();
            let raw = self.sb.raw;
            let block = self.cache.get_mut(&mut self.dev, 0)?;
            block[SB_OFFSET..SB_OFFSET + SB_SIZE].copy_from_slice(&raw);
            self.sb_dirty = false;
        }
        Ok(())
    }
}
