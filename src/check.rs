//! A consistency check of the whole file system, as far as this crate makes it (a small
//! `e2fsck -n`): every directory and file reached from the root, every block owned once, the
//! bitmaps, the counters. For tests after a power loss, on a device where e2fsck cannot run.

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use crate::dir::FT_DIR;
use crate::fs::{Ext4, Ino};
use crate::layout::*;
use crate::{BLOCK_SIZE, BlockDevice, ROOT_INO, Result};

/// Problems [`Ext4::check`] lists at most.
const MAX_PROBLEMS: usize = 32;

/// What [`Ext4::check`] found.
#[derive(Debug, Default, Clone)]
pub struct CheckReport {
    pub dirs: u32,
    pub files: u32,
    /// Bytes in files.
    pub bytes: u64,
    pub used_blocks: u64,
    pub used_inodes: u32,
    /// What is wrong, at most 32; empty when the file system is consistent.
    pub problems: Vec<String>,
}

impl CheckReport {
    fn problem(&mut self, p: String) {
        if self.problems.len() < MAX_PROBLEMS {
            self.problems.push(p);
        }
    }
}

fn bit(map: &[u8], i: u64) -> bool {
    map[(i / 8) as usize] & (1 << (i % 8)) != 0
}

impl<D: BlockDevice> Ext4<D> {
    /// Commits what is pending, then checks the whole file system. An `Err` is a structure too
    /// broken to go on (or the device failing); everything else is in the report.
    pub fn check(&mut self) -> Result<CheckReport> {
        self.sync()?;
        let mut r = CheckReport::default();
        let blocks = self.geo.blocks;
        let mut owned = vec![0u8; blocks.div_ceil(8) as usize];
        let mut claim = |r: &mut CheckReport, owner: Ino, start: u64, len: u32| {
            for b in start..start + len as u64 {
                if b >= blocks {
                    r.problem(format!("inode {} uses block {} past the end", owner, b));
                    return;
                }
                if bit(&owned, b) {
                    r.problem(format!("block {} used twice (inode {})", b, owner));
                } else {
                    owned[(b / 8) as usize] |= 1 << (b % 8);
                }
            }
        };
        for g in 0..self.geo.groups {
            claim(&mut r, 0, self.geo.group_start(g), self.geo.overhead(g));
        }

        // The tree, from the root, and the journal.
        let mut reached: BTreeSet<Ino> = BTreeSet::new();
        let mut refs: BTreeMap<Ino, u32> = BTreeMap::new();
        let mut dirs = vec![(ROOT_INO, ROOT_INO)];
        reached.insert(ROOT_INO);
        reached.insert(JOURNAL_INO);
        let journal = self.read_inode(JOURNAL_INO)?;
        self.extent_walk(JOURNAL_INO, &journal, &mut |start, len, _| {
            claim(&mut r, JOURNAL_INO, start, len);
            Ok(())
        })?;
        while let Some((ino, parent)) = dirs.pop() {
            r.dirs += 1;
            let inode = self.read_inode(ino)?;
            if !inode.is_dir() {
                r.problem(format!("inode {} is linked as a directory but is not one", ino));
                continue;
            }
            let mut used = 0u64;
            let end = self.extent_walk(ino, &inode, &mut |start, len, _| {
                used += len as u64;
                claim(&mut r, ino, start, len);
                Ok(())
            })?;
            if inode.size() != end as u64 * BLOCK_SIZE as u64 {
                r.problem(format!("directory {}: size {} for {} blocks", ino, inode.size(), end));
            }
            if inode.sectors() != used * 8 {
                r.problem(format!("directory {}: {} sectors for {} blocks", ino, inode.sectors(), used));
            }
            let mut entries: Vec<(Vec<u8>, Ino, u8)> = Vec::new();
            self.dir_for_each(ino, &inode, |name, child, ft| {
                entries.push((name.to_vec(), child, ft));
                true
            })?;
            let mut subdirs = 0u32;
            for (name, child, ft) in entries {
                if name == b"." {
                    if child != ino {
                        r.problem(format!("directory {}: . is {}", ino, child));
                    }
                    continue;
                }
                if name == b".." {
                    if child != parent {
                        r.problem(format!("directory {}: .. is {}, not {}", ino, child, parent));
                    }
                    continue;
                }
                if child < FIRST_INO && child != ROOT_INO || child > self.geo.total_inodes() {
                    r.problem(format!("directory {}: entry {} is inode {}", ino, String::from_utf8_lossy(&name), child));
                    continue;
                }
                if ft == FT_DIR {
                    subdirs += 1;
                    if !reached.insert(child) {
                        r.problem(format!("directory {} linked twice", child));
                        continue;
                    }
                    dirs.push((child, ino));
                } else {
                    *refs.entry(child).or_insert(0) += 1;
                }
            }
            if inode.links() as u32 != 2 + subdirs {
                r.problem(format!("directory {}: {} links, {} subdirectories", ino, inode.links(), subdirs));
            }
        }
        for (&ino, &count) in &refs {
            if !reached.insert(ino) {
                r.problem(format!("inode {} is both a directory and a file", ino));
                continue;
            }
            r.files += 1;
            let inode = self.read_inode(ino)?;
            if !inode.is_file() {
                r.problem(format!("inode {} is linked as a file but is not one", ino));
                continue;
            }
            if inode.links() as u32 != count {
                r.problem(format!("file {}: {} links, {} entries", ino, inode.links(), count));
            }
            let mut used = 0u64;
            let end = self.extent_walk(ino, &inode, &mut |start, len, _| {
                used += len as u64;
                claim(&mut r, ino, start, len);
                Ok(())
            })?;
            if end as u64 > inode.size().div_ceil(BLOCK_SIZE as u64) {
                r.problem(format!("file {}: blocks past its size {}", ino, inode.size()));
            }
            if inode.sectors() != used * 8 {
                r.problem(format!("file {}: {} sectors for {} blocks", ino, inode.sectors(), used));
            }
            r.bytes += inode.size();
        }

        // The bitmaps and the counters against what was reached.
        let ipg = self.geo.inodes_per_group;
        let (mut free_blocks, mut free_inodes) = (0u64, 0u32);
        for g in 0..self.geo.groups {
            let base = self.geo.group_start(g);
            let map = self.cache.get(&mut self.dev, self.geo.block_bitmap(g))?.to_vec();
            let mut free = 0u32;
            let mut wrong = 0u32;
            for i in 0..self.geo.group_blocks(g) {
                let on = bit(&map, i as u64);
                if !on {
                    free += 1;
                }
                if on != bit(&owned, base + i as u64) {
                    if wrong == 0 {
                        let what = if on { "used but owned by nothing" } else { "free but in use" };
                        r.problem(format!("group {}: block {} {}", g, base + i as u64, what));
                    }
                    wrong += 1;
                }
            }
            if wrong > 1 {
                r.problem(format!("group {}: {} blocks wrong in the bitmap", g, wrong));
            }
            if free != self.groups[g as usize].free_blocks() {
                r.problem(format!("group {}: {} free blocks, descriptor says {}", g, free, self.groups[g as usize].free_blocks()));
            }
            free_blocks += free as u64;

            let map = self.cache.get(&mut self.dev, self.geo.block_bitmap(g) + 1)?.to_vec();
            let mut free = 0u32;
            for i in 0..ipg {
                let ino = g * ipg + i + 1;
                let on = bit(&map, i as u64);
                if !on {
                    free += 1;
                }
                let want = ino < FIRST_INO || reached.contains(&ino);
                if on != want {
                    let what = if on { "used but not reached" } else { "in use but free in the bitmap" };
                    r.problem(format!("inode {} {}", ino, what));
                }
            }
            if free != self.groups[g as usize].free_inodes() {
                r.problem(format!("group {}: {} free inodes, descriptor says {}", g, free, self.groups[g as usize].free_inodes()));
            }
            free_inodes += free;
        }
        if free_blocks != self.sb.free_blocks() {
            r.problem(format!("{} free blocks, superblock says {}", free_blocks, self.sb.free_blocks()));
        }
        if free_inodes != self.sb.free_inodes() {
            r.problem(format!("{} free inodes, superblock says {}", free_inodes, self.sb.free_inodes()));
        }
        r.used_blocks = blocks - free_blocks;
        r.used_inodes = self.geo.total_inodes() - free_inodes;
        Ok(r)
    }
}
