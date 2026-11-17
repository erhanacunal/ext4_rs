//! The jbd2 journal (inode 8; all fields big-endian), checksum v3.
//!
//! [`Ext4::commit`] writes one transaction at the start of the log, commits it, writes its
//! blocks home and empties the log again:
//!
//! 1. flush: file data written so far is on the device;
//! 2. descriptor blocks and copies of the metadata blocks at log block 1..; flush;
//! 3. the commit block, and the journal superblock pointing at the transaction; flush;
//! 4. the metadata blocks at their homes; flush;
//! 5. the journal superblock marks the log empty; flush.
//!
//! A power loss before 3 is durable leaves the old state (homes untouched); after it, mount
//! replays the transaction. The log never holds two transactions, so no revoke records.

use alloc::vec;
use alloc::vec::Vec;

use crate::bytes::*;
use crate::crc32c::crc32c;
use crate::extent::root_extents;
use crate::fs::Ext4;
use crate::inode::Inode;
use crate::{BLOCK_SIZE, BlockDevice, Error, Result};

pub const MAGIC: u32 = 0xC03B_3998;
const DESCRIPTOR: u32 = 1;
const COMMIT: u32 = 2;
const SUPERBLOCK_V2: u32 = 4;
const REVOKE: u32 = 5;
pub const INCOMPAT_REVOKE: u32 = 0x1;
const INCOMPAT_CSUM_V2: u32 = 0x8;
pub const INCOMPAT_CSUM_V3: u32 = 0x10;
const CSUM_CRC32C: u8 = 4;

const FLAG_ESCAPE: u32 = 1;
const FLAG_SAME_UUID: u32 = 2;
const FLAG_LAST: u32 = 8;
const TAG: usize = 16;
/// Tags in a descriptor: header, the first tag with its UUID, the others, the tail.
const TAGS_PER_DESCRIPTOR: usize = 1 + (BLOCK_SIZE - 12 - 4 - (TAG + 16)) / TAG;

/// Journal superblock offsets.
mod jsb {
    pub const BLOCKSIZE: usize = 0xC;
    pub const MAXLEN: usize = 0x10;
    pub const FIRST: usize = 0x14;
    pub const SEQUENCE: usize = 0x18;
    pub const START: usize = 0x1C;
    pub const FEATURE_COMPAT: usize = 0x24;
    pub const FEATURE_INCOMPAT: usize = 0x28;
    pub const FEATURE_RO_COMPAT: usize = 0x2C;
    pub const UUID: usize = 0x30;
    pub const NR_USERS: usize = 0x40;
    pub const CHECKSUM_TYPE: usize = 0x50;
    pub const CHECKSUM: usize = 0xFC;
}

/// A new journal's superblock (used by [`crate::format`]).
pub fn new_superblock(maxlen: u32, uuid: &[u8; 16]) -> Vec<u8> {
    let mut b = vec![0u8; BLOCK_SIZE];
    set_be32(&mut b, 0, MAGIC);
    set_be32(&mut b, 4, SUPERBLOCK_V2);
    set_be32(&mut b, jsb::BLOCKSIZE, BLOCK_SIZE as u32);
    set_be32(&mut b, jsb::MAXLEN, maxlen);
    set_be32(&mut b, jsb::FIRST, 1);
    set_be32(&mut b, jsb::SEQUENCE, 1);
    set_be32(&mut b, jsb::START, 0);
    set_be32(&mut b, jsb::FEATURE_INCOMPAT, INCOMPAT_CSUM_V3);
    b[jsb::UUID..jsb::UUID + 16].copy_from_slice(uuid);
    set_be32(&mut b, jsb::NR_USERS, 1);
    b[jsb::CHECKSUM_TYPE] = CSUM_CRC32C;
    seal_superblock(&mut b);
    b
}

fn seal_superblock(b: &mut [u8]) {
    set_be32(b, jsb::CHECKSUM, 0);
    let c = crc32c(!0, &b[..1024]);
    set_be32(b, jsb::CHECKSUM, c);
}

fn header(b: &mut [u8], kind: u32, sequence: u32) {
    set_be32(b, 0, MAGIC);
    set_be32(b, 4, kind);
    set_be32(b, 8, sequence);
}

pub struct Journal {
    /// The journal file: (logical, physical, length).
    extents: Vec<(u32, u64, u32)>,
    maxlen: u32,
    first: u32,
    /// The next transaction's ID.
    sequence: u32,
    /// The superblock as last written.
    sb: Vec<u8>,
    /// crc32c of the journal UUID.
    seed: u32,
}

impl Journal {
    /// Reads the journal superblock through the journal inode.
    pub fn open<D: BlockDevice>(dev: &mut D, inode: &Inode, fs_uuid: &[u8; 16]) -> Result<Self> {
        let extents = root_extents(inode)?;
        let blocks: u64 = extents.iter().map(|e| e.2 as u64).sum();
        if blocks < 1024 || inode.size() != blocks * BLOCK_SIZE as u64 {
            return Err(Error::Corrupt("journal inode"));
        }
        let mut journal = Self { extents, maxlen: 0, first: 0, sequence: 0, sb: vec![0; BLOCK_SIZE], seed: 0 };
        let mut sb = vec![0u8; BLOCK_SIZE];
        dev.read(journal.pblk(0)?, &mut sb)?;
        if be32(&sb, 0) != MAGIC || be32(&sb, 4) != SUPERBLOCK_V2 {
            return Err(Error::Corrupt("journal superblock"));
        }
        let incompat = be32(&sb, jsb::FEATURE_INCOMPAT);
        if incompat & (INCOMPAT_CSUM_V2 | INCOMPAT_CSUM_V3) == 0 {
            // mke2fs makes journals without checksums; Linux turns v3 on at the first mount,
            // and so does this (only an empty one: one with data is not checked).
            if be32(&sb, jsb::START) != 0 {
                return Err(Error::Unsupported("journal without checksums holds data"));
            }
            set_be32(&mut sb, jsb::FEATURE_INCOMPAT, incompat | INCOMPAT_CSUM_V3);
            sb[jsb::CHECKSUM_TYPE] = CSUM_CRC32C;
            seal_superblock(&mut sb);
        }
        let check = {
            let mut copy = sb.clone();
            seal_superblock(&mut copy);
            be32(&copy, jsb::CHECKSUM)
        };
        if be32(&sb, jsb::CHECKSUM) != check {
            return Err(Error::Corrupt("journal superblock checksum"));
        }
        if be32(&sb, jsb::FEATURE_INCOMPAT) & !INCOMPAT_REVOKE != INCOMPAT_CSUM_V3
            || be32(&sb, jsb::FEATURE_RO_COMPAT) != 0
            || be32(&sb, jsb::FEATURE_COMPAT) != 0
            || sb[jsb::CHECKSUM_TYPE] != CSUM_CRC32C
            || be32(&sb, jsb::BLOCKSIZE) != BLOCK_SIZE as u32
        {
            return Err(Error::Unsupported("journal features"));
        }
        journal.maxlen = be32(&sb, jsb::MAXLEN);
        journal.first = be32(&sb, jsb::FIRST);
        if journal.maxlen as u64 > blocks || journal.first == 0 || journal.first >= journal.maxlen {
            return Err(Error::Corrupt("journal geometry"));
        }
        if &sb[jsb::UUID..jsb::UUID + 16] != fs_uuid {
            return Err(Error::Corrupt("journal UUID"));
        }
        journal.sequence = be32(&sb, jsb::SEQUENCE);
        journal.seed = crc32c(!0, &sb[jsb::UUID..jsb::UUID + 16]);
        journal.sb = sb;
        Ok(journal)
    }

    /// Blocks one transaction may hold (a commit needs its descriptors and a commit block).
    pub fn capacity(&self) -> usize {
        let log = (self.maxlen - self.first) as usize;
        // n + ceil(n / TAGS) + 1 <= log
        (log - 1) * TAGS_PER_DESCRIPTOR / (TAGS_PER_DESCRIPTOR + 1)
    }

    fn pblk(&self, lblk: u32) -> Result<u64> {
        for &(l, p, n) in &self.extents {
            if lblk >= l && lblk - l < n {
                return Ok(p + (lblk - l) as u64);
            }
        }
        Err(Error::Corrupt("journal block unmapped"))
    }

    /// Writes `data` (whole blocks) at log blocks `lblk..`, split where the journal file is.
    fn write_log<D: BlockDevice>(&self, dev: &mut D, lblk: u32, data: &[u8]) -> Result<()> {
        let mut done = 0;
        let mut l = lblk;
        while done < data.len() {
            let p = self.pblk(l)?;
            // Contiguous blocks from `p`.
            let mut n = 1;
            while done + n * BLOCK_SIZE < data.len() && self.pblk(l + n as u32)? == p + n as u64 {
                n += 1;
            }
            dev.write(p, &data[done..done + n * BLOCK_SIZE])?;
            done += n * BLOCK_SIZE;
            l += n as u32;
        }
        Ok(())
    }

    fn write_superblock<D: BlockDevice>(&mut self, dev: &mut D, start: u32) -> Result<()> {
        set_be32(&mut self.sb, jsb::SEQUENCE, self.sequence);
        set_be32(&mut self.sb, jsb::START, start);
        seal_superblock(&mut self.sb);
        dev.write(self.pblk(0)?, &self.sb)?;
        Ok(())
    }

    /// Steps 1–3 of a commit: the transaction is in the log and committed.
    pub fn log<D: BlockDevice>(&mut self, dev: &mut D, blocks: &[(u64, &[u8])], now: u32) -> Result<()> {
        dev.flush()?;
        let tid = self.sequence;
        let mut lblk = self.first;
        for chunk in blocks.chunks(TAGS_PER_DESCRIPTOR) {
            let mut buf = vec![0u8; (1 + chunk.len()) * BLOCK_SIZE];
            let (desc, copies) = buf.split_at_mut(BLOCK_SIZE);
            header(desc, DESCRIPTOR, tid);
            let mut at = 12;
            for (i, &(nr, data)) in chunk.iter().enumerate() {
                if nr > u32::MAX as u64 {
                    return Err(Error::Unsupported("block number past 32 bits"));
                }
                let copy = &mut copies[i * BLOCK_SIZE..(i + 1) * BLOCK_SIZE];
                copy.copy_from_slice(data);
                let mut flags = 0;
                if be32(copy, 0) == MAGIC {
                    copy[..4].fill(0);
                    flags |= FLAG_ESCAPE;
                }
                if i > 0 {
                    flags |= FLAG_SAME_UUID;
                }
                if i == chunk.len() - 1 {
                    flags |= FLAG_LAST;
                }
                let c = crc32c(crc32c(self.seed, &tid.to_be_bytes()), copy);
                set_be32(desc, at, nr as u32);
                set_be32(desc, at + 4, flags);
                set_be32(desc, at + 8, 0);
                set_be32(desc, at + 12, c);
                at += TAG;
                if i == 0 {
                    desc[at..at + 16].copy_from_slice(&self.sb[jsb::UUID..jsb::UUID + 16]);
                    at += 16;
                }
            }
            let c = crc32c(self.seed, desc);
            set_be32(desc, BLOCK_SIZE - 4, c);
            self.write_log(dev, lblk, &buf)?;
            lblk += 1 + chunk.len() as u32;
        }
        dev.flush()?;

        let mut commit = vec![0u8; BLOCK_SIZE];
        header(&mut commit, COMMIT, tid);
        set_be64(&mut commit, 0x30, now as u64);
        let c = crc32c(self.seed, &commit);
        set_be32(&mut commit, 0x10, c);
        self.write_log(dev, lblk, &commit)?;
        self.write_superblock(dev, self.first)?;
        dev.flush()?;
        Ok(())
    }

    /// Step 5: the transaction is home; the log is empty.
    pub fn empty<D: BlockDevice>(&mut self, dev: &mut D) -> Result<()> {
        self.sequence = self.sequence.wrapping_add(1);
        self.write_superblock(dev, 0)?;
        dev.flush()?;
        Ok(())
    }

    /// Replays the committed transactions in the log, then empties it. Returns whether
    /// anything was written.
    pub fn replay<D: BlockDevice>(&mut self, dev: &mut D) -> Result<bool> {
        let start = be32(&self.sb, jsb::START);
        if start == 0 {
            return Ok(false);
        }
        if start < self.first || start >= self.maxlen {
            return Err(Error::Corrupt("journal start"));
        }
        let log_len = self.maxlen - self.first;
        let next = |l: u32| if l + 1 >= self.maxlen { self.first } else { l + 1 };
        let mut pos = start;
        let mut seq = self.sequence;
        let mut applied = false;
        let mut block = vec![0u8; BLOCK_SIZE];
        let mut walked = 0u32;
        // The current transaction's blocks: (home, log block, flags, checksum).
        let mut pending: Vec<(u64, u32, u32, u32)> = Vec::new();
        'scan: loop {
            if walked > log_len {
                break;
            }
            dev.read(self.pblk(pos)?, &mut block)?;
            walked += 1;
            if be32(&block, 0) != MAGIC || be32(&block, 8) != seq {
                break;
            }
            match be32(&block, 4) {
                DESCRIPTOR => {
                    let mut copy = block.clone();
                    set_be32(&mut copy, BLOCK_SIZE - 4, 0);
                    if crc32c(self.seed, &copy) != be32(&block, BLOCK_SIZE - 4) {
                        break;
                    }
                    let mut at = 12;
                    let mut log = next(pos);
                    loop {
                        if at + TAG > BLOCK_SIZE - 4 {
                            break 'scan;
                        }
                        let nr = be32(&block, at) as u64 | (be32(&block, at + 8) as u64) << 32;
                        let flags = be32(&block, at + 4);
                        let csum = be32(&block, at + 12);
                        pending.push((nr, log, flags, csum));
                        log = next(log);
                        walked += 1;
                        at += TAG;
                        if flags & FLAG_SAME_UUID == 0 {
                            at += 16;
                        }
                        if flags & FLAG_LAST != 0 {
                            break;
                        }
                    }
                    pos = log;
                }
                COMMIT => {
                    let mut copy = block.clone();
                    set_be32(&mut copy, 0x10, 0);
                    if crc32c(self.seed, &copy) != be32(&block, 0x10) {
                        break;
                    }
                    for &(nr, log, flags, csum) in &pending {
                        dev.read(self.pblk(log)?, &mut block)?;
                        if crc32c(crc32c(self.seed, &seq.to_be_bytes()), &block) != csum {
                            // As Linux: a block that does not match is skipped.
                            continue;
                        }
                        if flags & FLAG_ESCAPE != 0 {
                            set_be32(&mut block, 0, MAGIC);
                        }
                        dev.write(nr, &block)?;
                        applied = true;
                    }
                    pending.clear();
                    seq = seq.wrapping_add(1);
                    pos = next(pos);
                }
                REVOKE => return Err(Error::Unsupported("journal revoke records")),
                _ => break,
            }
        }
        dev.flush()?;
        self.sequence = seq;
        self.write_superblock(dev, 0)?;
        dev.flush()?;
        Ok(applied)
    }
}

impl<D: BlockDevice> Ext4<D> {
    /// Commits the running transaction (see the module) and flushes the device.
    pub(crate) fn commit(&mut self) -> Result<()> {
        self.writable()?;
        let result = self.commit_inner();
        if result.is_err() {
            self.failed = true;
            self.cache.discard_dirty();
        }
        result
    }

    /// For power-loss tests: commits the running transaction to the log only, as a power cut
    /// right after the commit block would leave it, and makes the file system read-only. The
    /// next mount must replay it.
    pub fn commit_to_log_only(&mut self) -> Result<()> {
        self.writable()?;
        let result = (|| {
            self.stage_counters()?;
            let now = self.now();
            let blocks: Vec<(u64, &[u8])> = self.cache.dirty_blocks().collect();
            if blocks.is_empty() {
                return Ok(());
            }
            self.journal.log(&mut self.dev, &blocks, now)
        })();
        self.failed = true;
        self.cache.discard_dirty();
        result
    }

    fn commit_inner(&mut self) -> Result<()> {
        self.stage_counters()?;
        if self.cache.dirty_count() == 0 {
            self.dev.flush()?;
            self.freed.clear();
            self.committed_at = self.cache.changes;
            return Ok(());
        }
        let now = self.now();
        let blocks: Vec<(u64, &[u8])> = self.cache.dirty_blocks().collect();
        self.journal.log(&mut self.dev, &blocks, now)?;
        // Home, in runs of consecutive blocks.
        let mut i = 0;
        let mut run = Vec::new();
        while i < blocks.len() {
            let start = blocks[i].0;
            run.clear();
            run.extend_from_slice(blocks[i].1);
            let mut j = i + 1;
            while j < blocks.len() && blocks[j].0 == start + (j - i) as u64 && j - i < 64 {
                run.extend_from_slice(blocks[j].1);
                j += 1;
            }
            self.dev.write(start, &run)?;
            i = j;
        }
        self.dev.flush()?;
        drop(blocks);
        self.journal.empty(&mut self.dev)?;
        self.cache.mark_clean();
        self.freed.clear();
        self.committed_at = self.cache.changes;
        Ok(())
    }
}
