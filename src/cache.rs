//! The metadata block cache. Every metadata block (bitmaps, inode tables, directory and
//! extent tree blocks, block 0 with the superblock, the descriptor table) is read and changed
//! here; a changed block is dirty, belongs to the running transaction and stays until the
//! transaction commits ([`crate::journal`]). Clean blocks go least recently used first.
//!
//! File data never comes here: it goes straight between the caller and the device.

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::vec;

use crate::{BLOCK_SIZE, BlockDevice, Result};

struct Slot {
    data: Box<[u8]>,
    dirty: bool,
    used: u64,
}

pub struct Cache {
    map: BTreeMap<u64, Slot>,
    tick: u64,
    /// Clean blocks kept at most (dirty ones are never dropped).
    cap: usize,
    dirty: usize,
    /// Changes made so far (dirtied, zeroed or forgotten blocks).
    pub changes: u64,
}

impl Cache {
    pub fn new(cap: usize) -> Self {
        Self { map: BTreeMap::new(), tick: 0, cap: cap.max(16), dirty: 0, changes: 0 }
    }

    /// Drops the least recently used clean block once the clean ones reach the cap.
    fn make_room(&mut self) {
        if self.map.len() - self.dirty < self.cap {
            return;
        }
        let oldest = self.map.iter().filter(|(_, s)| !s.dirty).min_by_key(|(_, s)| s.used).map(|(&nr, _)| nr);
        if let Some(nr) = oldest {
            self.map.remove(&nr);
        }
    }

    fn slot<D: BlockDevice>(&mut self, dev: &mut D, nr: u64) -> Result<&mut Slot> {
        self.tick += 1;
        if !self.map.contains_key(&nr) {
            self.make_room();
            let mut data = vec![0u8; BLOCK_SIZE].into_boxed_slice();
            dev.read(nr, &mut data)?;
            self.map.insert(nr, Slot { data, dirty: false, used: 0 });
        }
        let slot = self.map.get_mut(&nr).unwrap();
        slot.used = self.tick;
        Ok(slot)
    }

    /// Block `nr`, read from the device if not here.
    pub fn get<D: BlockDevice>(&mut self, dev: &mut D, nr: u64) -> Result<&[u8]> {
        Ok(&self.slot(dev, nr)?.data)
    }

    /// Block `nr` to change: dirty from now on.
    pub fn get_mut<D: BlockDevice>(&mut self, dev: &mut D, nr: u64) -> Result<&mut [u8]> {
        self.changes += 1;
        let slot = self.slot(dev, nr)?;
        if !slot.dirty {
            slot.dirty = true;
            self.dirty += 1;
        }
        Ok(&mut self.map.get_mut(&nr).unwrap().data)
    }

    /// Block `nr`, newly allocated: zeroed and dirty, not read.
    pub fn zeroed(&mut self, nr: u64) -> &mut [u8] {
        self.changes += 1;
        self.tick += 1;
        let tick = self.tick;
        if let Some(slot) = self.map.get_mut(&nr) {
            if !slot.dirty {
                self.dirty += 1;
            }
            slot.data.fill(0);
            slot.dirty = true;
            slot.used = tick;
        } else {
            self.make_room();
            let data = vec![0u8; BLOCK_SIZE].into_boxed_slice();
            self.map.insert(nr, Slot { data, dirty: true, used: tick });
            self.dirty += 1;
        }
        &mut self.map.get_mut(&nr).unwrap().data
    }

    /// Block `nr` was freed: whatever was here (dirty or not) must never reach the device.
    pub fn forget(&mut self, nr: u64) {
        self.changes += 1;
        if let Some(slot) = self.map.remove(&nr) {
            if slot.dirty {
                self.dirty -= 1;
            }
        }
    }

    /// Drops block `nr` if it is here and clean (the device has newer contents).
    pub fn invalidate(&mut self, nr: u64) {
        if self.map.get(&nr).is_some_and(|s| !s.dirty) {
            self.map.remove(&nr);
        }
    }

    /// Forgets the blocks `start..start + len` that are here.
    pub fn forget_range(&mut self, start: u64, len: u64) {
        let nrs: alloc::vec::Vec<u64> = self.map.range(start..start + len).map(|(&nr, _)| nr).collect();
        for nr in nrs {
            self.forget(nr);
        }
    }

    pub fn dirty_count(&self) -> usize {
        self.dirty
    }

    /// The dirty blocks, by block number.
    pub fn dirty_blocks(&self) -> impl Iterator<Item = (u64, &[u8])> {
        self.map.iter().filter(|(_, s)| s.dirty).map(|(&nr, s)| (nr, &s.data[..]))
    }

    /// The transaction reached the device: every block is clean.
    pub fn mark_clean(&mut self) {
        for slot in self.map.values_mut() {
            slot.dirty = false;
        }
        self.dirty = 0;
    }

    /// Drops the dirty blocks (after a failed write: they will never be committed).
    pub fn discard_dirty(&mut self) {
        self.map.retain(|_, s| !s.dirty);
        self.dirty = 0;
    }

    /// Drops everything (after a journal replay rewrote blocks behind the cache).
    pub fn clear(&mut self) {
        self.map.clear();
        self.dirty = 0;
    }
}
