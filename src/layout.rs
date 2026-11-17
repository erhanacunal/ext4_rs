//! On-disk layout: the superblock, group descriptors and the checksums of both, and the
//! constants of the one shape of file system this crate makes.

use crate::bytes::*;
use crate::crc32c::crc32c;
use crate::{BLOCK_SIZE, Error, Result};

pub const BLOCKS_PER_GROUP: u32 = 32768;
pub const INODE_SIZE: usize = 256;
/// `i_extra_isize`: the extra fields up to and including `i_crtime_extra`.
pub const EXTRA_ISIZE: u16 = 32;
pub const DESC_SIZE: usize = 32;
pub const DESCS_PER_BLOCK: u32 = (BLOCK_SIZE / DESC_SIZE) as u32;
pub const INODES_PER_BLOCK: u32 = (BLOCK_SIZE / INODE_SIZE) as u32;
/// The first inode files may use; 1–10 are reserved, 11 is `lost+found`.
pub const FIRST_INO: u32 = 11;
pub const JOURNAL_INO: u32 = 8;
pub const LOST_FOUND_INO: u32 = 11;
/// Byte offset of the primary superblock in block 0.
pub const SB_OFFSET: usize = 1024;
pub const SB_SIZE: usize = 1024;
pub const MAGIC: u16 = 0xEF53;

pub const COMPAT_HAS_JOURNAL: u32 = 0x4;
pub const INCOMPAT_FILETYPE: u32 = 0x2;
pub const INCOMPAT_RECOVER: u32 = 0x4;
pub const INCOMPAT_EXTENTS: u32 = 0x40;
pub const RO_COMPAT_SPARSE_SUPER: u32 = 0x1;
pub const RO_COMPAT_LARGE_FILE: u32 = 0x2;
pub const RO_COMPAT_DIR_NLINK: u32 = 0x20;
pub const RO_COMPAT_EXTRA_ISIZE: u32 = 0x40;
pub const RO_COMPAT_METADATA_CSUM: u32 = 0x400;

/// What [`crate::format`] sets.
pub const COMPAT: u32 = COMPAT_HAS_JOURNAL;
pub const INCOMPAT: u32 = INCOMPAT_FILETYPE | INCOMPAT_EXTENTS;
pub const RO_COMPAT: u32 = RO_COMPAT_SPARSE_SUPER
    | RO_COMPAT_LARGE_FILE
    | RO_COMPAT_DIR_NLINK
    | RO_COMPAT_EXTRA_ISIZE
    | RO_COMPAT_METADATA_CSUM;
/// Compat features a mount accepts besides [`COMPAT`]: they need nothing from a writer.
/// 0x8 ext_attr (no attributes are written), 0x20 dir_index (directories here never get the
/// index flag, so an index is never met).
pub const COMPAT_IGNORED: u32 = 0x8 | 0x20;

/// Group descriptor flags: the inode bitmap is not initialized (no inode used), the block
/// bitmap is not (only the group's metadata used), the inode table is zeroed.
pub const BG_INODE_UNINIT: u16 = 0x1;
pub const BG_BLOCK_UNINIT: u16 = 0x2;
pub const BG_INODE_ZEROED: u16 = 0x4;

/// Superblock field offsets.
pub mod sb {
    pub const INODES_COUNT: usize = 0x0;
    pub const BLOCKS_COUNT: usize = 0x4;
    pub const R_BLOCKS_COUNT: usize = 0x8;
    pub const FREE_BLOCKS: usize = 0xC;
    pub const FREE_INODES: usize = 0x10;
    pub const FIRST_DATA_BLOCK: usize = 0x14;
    pub const LOG_BLOCK_SIZE: usize = 0x18;
    pub const LOG_CLUSTER_SIZE: usize = 0x1C;
    pub const BLOCKS_PER_GROUP: usize = 0x20;
    pub const CLUSTERS_PER_GROUP: usize = 0x24;
    pub const INODES_PER_GROUP: usize = 0x28;
    pub const MTIME: usize = 0x2C;
    pub const WTIME: usize = 0x30;
    pub const MNT_COUNT: usize = 0x34;
    pub const MAX_MNT_COUNT: usize = 0x36;
    pub const MAGIC: usize = 0x38;
    pub const STATE: usize = 0x3A;
    pub const ERRORS: usize = 0x3C;
    pub const LASTCHECK: usize = 0x40;
    pub const REV_LEVEL: usize = 0x4C;
    pub const FIRST_INO: usize = 0x54;
    pub const INODE_SIZE: usize = 0x58;
    pub const BLOCK_GROUP_NR: usize = 0x5A;
    pub const FEATURE_COMPAT: usize = 0x5C;
    pub const FEATURE_INCOMPAT: usize = 0x60;
    pub const FEATURE_RO_COMPAT: usize = 0x64;
    pub const UUID: usize = 0x68;
    pub const VOLUME_NAME: usize = 0x78;
    pub const JOURNAL_INUM: usize = 0xE0;
    pub const HASH_SEED: usize = 0xEC;
    pub const DEF_HASH_VERSION: usize = 0xFC;
    pub const JNL_BACKUP_TYPE: usize = 0xFD;
    pub const MKFS_TIME: usize = 0x108;
    pub const JNL_BLOCKS: usize = 0x10C;
    pub const BLOCKS_COUNT_HI: usize = 0x150;
    pub const MIN_EXTRA_ISIZE: usize = 0x15C;
    pub const WANT_EXTRA_ISIZE: usize = 0x15E;
    pub const FLAGS: usize = 0x160;
    pub const CHECKSUM_TYPE: usize = 0x175;
    pub const CHECKSUM: usize = 0x3FC;
}

/// Group descriptor field offsets (32-byte descriptors).
pub mod gd {
    pub const BLOCK_BITMAP: usize = 0x0;
    pub const INODE_BITMAP: usize = 0x4;
    pub const INODE_TABLE: usize = 0x8;
    pub const FREE_BLOCKS: usize = 0xC;
    pub const FREE_INODES: usize = 0xE;
    pub const USED_DIRS: usize = 0x10;
    pub const FLAGS: usize = 0x12;
    pub const BLOCK_BITMAP_CSUM: usize = 0x18;
    pub const INODE_BITMAP_CSUM: usize = 0x1A;
    pub const ITABLE_UNUSED: usize = 0x1C;
    pub const CHECKSUM: usize = 0x1E;
}

/// Whether group `g` holds a superblock and descriptor copy (`sparse_super`: 0, 1 and the
/// powers of 3, 5 and 7).
pub fn has_super(g: u32) -> bool {
    if g <= 1 {
        return true;
    }
    for base in [3u32, 5, 7] {
        let mut p = base;
        while p < g {
            p = match p.checked_mul(base) {
                Some(p) => p,
                None => break,
            };
        }
        if p == g {
            return true;
        }
    }
    false
}

/// The geometry every part of the crate derives from the superblock.
#[derive(Clone, Copy, Debug)]
pub struct Geometry {
    pub blocks: u64,
    pub groups: u32,
    pub inodes_per_group: u32,
    /// Blocks of each group's inode table.
    pub itable_blocks: u32,
    /// Blocks of the descriptor table.
    pub gdt_blocks: u32,
}

impl Geometry {
    pub fn new(blocks: u64, inodes_per_group: u32) -> Self {
        let groups = blocks.div_ceil(BLOCKS_PER_GROUP as u64) as u32;
        Self {
            blocks,
            groups,
            inodes_per_group,
            itable_blocks: inodes_per_group / INODES_PER_BLOCK,
            gdt_blocks: groups.div_ceil(DESCS_PER_BLOCK),
        }
    }

    pub fn group_start(&self, g: u32) -> u64 {
        g as u64 * BLOCKS_PER_GROUP as u64
    }

    /// Blocks in group `g` (the last one may be short).
    pub fn group_blocks(&self, g: u32) -> u32 {
        (self.blocks - self.group_start(g)).min(BLOCKS_PER_GROUP as u64) as u32
    }

    /// The superblock copy and descriptor table at the start of group `g`, if it has them.
    pub fn super_blocks(&self, g: u32) -> u32 {
        if has_super(g) { 1 + self.gdt_blocks } else { 0 }
    }

    /// Where the block bitmap, inode bitmap and inode table of group `g` are (in that order,
    /// right after its superblock copy).
    pub fn block_bitmap(&self, g: u32) -> u64 {
        self.group_start(g) + self.super_blocks(g) as u64
    }

    /// Blocks at the start of group `g` that hold metadata.
    pub fn overhead(&self, g: u32) -> u32 {
        self.super_blocks(g) + 2 + self.itable_blocks
    }

    pub fn total_inodes(&self) -> u32 {
        self.groups * self.inodes_per_group
    }
}

/// The superblock, kept in memory; [`Superblock::write_to`] puts it (checksummed) in block 0.
#[derive(Clone)]
pub struct Superblock {
    pub raw: [u8; SB_SIZE],
}

impl Superblock {
    pub fn u32(&self, at: usize) -> u32 {
        le32(&self.raw, at)
    }

    pub fn set_u32(&mut self, at: usize, v: u32) {
        set_le32(&mut self.raw, at, v)
    }

    pub fn u16(&self, at: usize) -> u16 {
        le16(&self.raw, at)
    }

    pub fn set_u16(&mut self, at: usize, v: u16) {
        set_le16(&mut self.raw, at, v)
    }

    pub fn uuid(&self) -> [u8; 16] {
        self.raw[sb::UUID..sb::UUID + 16].try_into().unwrap()
    }

    pub fn blocks_count(&self) -> u64 {
        self.u32(sb::BLOCKS_COUNT) as u64
    }

    pub fn free_blocks(&self) -> u64 {
        self.u32(sb::FREE_BLOCKS) as u64
    }

    pub fn set_free_blocks(&mut self, v: u64) {
        self.set_u32(sb::FREE_BLOCKS, v as u32)
    }

    pub fn free_inodes(&self) -> u32 {
        self.u32(sb::FREE_INODES)
    }

    pub fn set_free_inodes(&mut self, v: u32) {
        self.set_u32(sb::FREE_INODES, v)
    }

    pub fn checksum(&self) -> u32 {
        crc32c(!0, &self.raw[..sb::CHECKSUM])
    }

    pub fn set_checksum(&mut self) {
        let c = self.checksum();
        self.set_u32(sb::CHECKSUM, c)
    }

    /// The checksum seed of every other structure: crc32c of the UUID.
    pub fn csum_seed(&self) -> u32 {
        crc32c(!0, &self.raw[sb::UUID..sb::UUID + 16])
    }

    /// Checks that this is a file system this crate makes; returns its geometry.
    pub fn check(&self, device_blocks: u64) -> Result<Geometry> {
        if self.u16(sb::MAGIC) != MAGIC {
            return Err(Error::Unsupported("no ext4 superblock"));
        }
        if self.u32(sb::CHECKSUM) != self.checksum() {
            return Err(Error::Corrupt("superblock checksum"));
        }
        let incompat = self.u32(sb::FEATURE_INCOMPAT);
        let ro = self.u32(sb::FEATURE_RO_COMPAT);
        let compat = self.u32(sb::FEATURE_COMPAT);
        if incompat & !(INCOMPAT | INCOMPAT_RECOVER) != 0 || incompat & INCOMPAT != INCOMPAT {
            return Err(Error::Unsupported("incompat features"));
        }
        if ro & !RO_COMPAT != 0 || ro & RO_COMPAT_METADATA_CSUM == 0 {
            return Err(Error::Unsupported("ro_compat features"));
        }
        if compat & !(COMPAT | COMPAT_IGNORED) != 0 || compat & COMPAT_HAS_JOURNAL == 0 {
            return Err(Error::Unsupported("compat features"));
        }
        if self.u32(sb::REV_LEVEL) != 1
            || self.u32(sb::LOG_BLOCK_SIZE) != 2
            || self.u32(sb::LOG_CLUSTER_SIZE) != 2
            || self.u32(sb::FIRST_DATA_BLOCK) != 0
            || self.u32(sb::BLOCKS_PER_GROUP) != BLOCKS_PER_GROUP
            || self.u16(sb::INODE_SIZE) as usize != INODE_SIZE
            || self.u32(sb::FIRST_INO) != FIRST_INO
            || self.u32(sb::JOURNAL_INUM) != JOURNAL_INO
            || self.raw[sb::CHECKSUM_TYPE] != 1
            || self.u32(sb::BLOCKS_COUNT_HI) != 0
        {
            return Err(Error::Unsupported("geometry"));
        }
        let ipg = self.u32(sb::INODES_PER_GROUP);
        if ipg == 0 || ipg % INODES_PER_BLOCK != 0 || ipg > BLOCKS_PER_GROUP {
            return Err(Error::Unsupported("inodes per group"));
        }
        let geo = Geometry::new(self.blocks_count(), ipg);
        if geo.blocks > device_blocks {
            return Err(Error::Corrupt("file system larger than the device"));
        }
        if self.u32(sb::INODES_COUNT) != geo.total_inodes() {
            return Err(Error::Corrupt("inode count"));
        }
        Ok(geo)
    }
}

/// One group's descriptor, kept in memory.
#[derive(Clone, Copy)]
pub struct GroupDesc {
    pub raw: [u8; DESC_SIZE],
}

impl GroupDesc {
    pub fn u32(&self, at: usize) -> u32 {
        le32(&self.raw, at)
    }

    pub fn u16(&self, at: usize) -> u16 {
        le16(&self.raw, at)
    }

    pub fn set_u32(&mut self, at: usize, v: u32) {
        set_le32(&mut self.raw, at, v)
    }

    pub fn set_u16(&mut self, at: usize, v: u16) {
        set_le16(&mut self.raw, at, v)
    }

    pub fn free_blocks(&self) -> u32 {
        self.u16(gd::FREE_BLOCKS) as u32
    }

    pub fn free_inodes(&self) -> u32 {
        self.u16(gd::FREE_INODES) as u32
    }

    /// The descriptor's checksum (`metadata_csum`: crc32c of the seed, the group number and
    /// the descriptor with this field zero, low 16 bits).
    pub fn checksum(&self, seed: u32, group: u32) -> u16 {
        let mut copy = self.raw;
        set_le16(&mut copy, gd::CHECKSUM, 0);
        let c = crc32c(seed, &group.to_le_bytes());
        crc32c(c, &copy) as u16
    }

    pub fn set_checksum(&mut self, seed: u32, group: u32) {
        let c = self.checksum(seed, group);
        self.set_u16(gd::CHECKSUM, c)
    }
}

/// Checksum of a bitmap of `bits` bits (low 16 bits go in the descriptor).
pub fn bitmap_checksum(seed: u32, bitmap: &[u8], bits: u32) -> u16 {
    crc32c(seed, &bitmap[..(bits / 8) as usize]) as u16
}

#[cfg(test)]
mod tests {
    use super::has_super;

    #[test]
    fn sparse_groups() {
        let with: alloc::vec::Vec<u32> = (0..60).filter(|&g| has_super(g)).collect();
        assert_eq!(with, [0, 1, 3, 5, 7, 9, 25, 27, 49]);
    }
}
