//! One inode (256 bytes): its fields and checksum.

use crate::bytes::*;
use crate::crc32c::crc32c;
use crate::layout::{EXTRA_ISIZE, INODE_SIZE};

pub const S_IFMT: u16 = 0xF000;
pub const S_IFREG: u16 = 0x8000;
pub const S_IFDIR: u16 = 0x4000;
/// `i_flags`: the inode maps its blocks with an extent tree.
pub const EXTENTS_FL: u32 = 0x80000;
/// `i_flags`: a hashed (htree) directory, which this crate cannot update.
pub const INDEX_FL: u32 = 0x1000;

mod off {
    pub const MODE: usize = 0x0;
    pub const SIZE_LO: usize = 0x4;
    pub const ATIME: usize = 0x8;
    pub const CTIME: usize = 0xC;
    pub const MTIME: usize = 0x10;
    pub const DTIME: usize = 0x14;
    pub const LINKS: usize = 0x1A;
    pub const BLOCKS_LO: usize = 0x1C;
    pub const FLAGS: usize = 0x20;
    pub const BLOCK: usize = 0x28;
    pub const GENERATION: usize = 0x64;
    pub const SIZE_HI: usize = 0x6C;
    pub const BLOCKS_HI: usize = 0x74;
    pub const CHECKSUM_LO: usize = 0x7C;
    pub const EXTRA_ISIZE: usize = 0x80;
    pub const CHECKSUM_HI: usize = 0x82;
    pub const CRTIME: usize = 0x90;
}

/// The `i_block` area: the root of the extent tree.
pub const I_BLOCK: core::ops::Range<usize> = off::BLOCK..off::BLOCK + 60;

#[derive(Clone)]
pub struct Inode {
    pub raw: [u8; INODE_SIZE],
}

impl Inode {
    pub fn zeroed() -> Self {
        Self { raw: [0; INODE_SIZE] }
    }

    /// A new inode of `mode`, timestamps `now`, links 1, an empty extent tree.
    pub fn new(mode: u16, now: u32, generation: u32) -> Self {
        let mut i = Self::zeroed();
        set_le16(&mut i.raw, off::MODE, mode);
        for at in [off::ATIME, off::CTIME, off::MTIME, off::CRTIME] {
            set_le32(&mut i.raw, at, now);
        }
        set_le16(&mut i.raw, off::LINKS, 1);
        set_le32(&mut i.raw, off::FLAGS, EXTENTS_FL);
        set_le32(&mut i.raw, off::GENERATION, generation);
        set_le16(&mut i.raw, off::EXTRA_ISIZE, EXTRA_ISIZE);
        crate::extent::init_root(i.i_block_mut());
        i
    }

    pub fn mode(&self) -> u16 {
        le16(&self.raw, off::MODE)
    }

    pub fn is_dir(&self) -> bool {
        self.mode() & S_IFMT == S_IFDIR
    }

    pub fn is_file(&self) -> bool {
        self.mode() & S_IFMT == S_IFREG
    }

    pub fn size(&self) -> u64 {
        le32(&self.raw, off::SIZE_LO) as u64 | (le32(&self.raw, off::SIZE_HI) as u64) << 32
    }

    pub fn set_size(&mut self, size: u64) {
        set_le32(&mut self.raw, off::SIZE_LO, size as u32);
        set_le32(&mut self.raw, off::SIZE_HI, (size >> 32) as u32);
    }

    pub fn links(&self) -> u16 {
        le16(&self.raw, off::LINKS)
    }

    pub fn set_links(&mut self, n: u16) {
        set_le16(&mut self.raw, off::LINKS, n)
    }

    /// `i_blocks`: 512-byte sectors held (data and extent tree blocks).
    pub fn sectors(&self) -> u64 {
        le32(&self.raw, off::BLOCKS_LO) as u64 | (le16(&self.raw, off::BLOCKS_HI) as u64) << 32
    }

    pub fn add_blocks(&mut self, blocks: i64) {
        let s = (self.sectors() as i64 + blocks * 8) as u64;
        set_le32(&mut self.raw, off::BLOCKS_LO, s as u32);
        set_le16(&mut self.raw, off::BLOCKS_HI, (s >> 32) as u16);
    }

    pub fn flags(&self) -> u32 {
        le32(&self.raw, off::FLAGS)
    }

    pub fn generation(&self) -> u32 {
        le32(&self.raw, off::GENERATION)
    }

    pub fn mtime(&self) -> u32 {
        le32(&self.raw, off::MTIME)
    }

    /// Marks a change of contents: mtime and ctime.
    pub fn touch(&mut self, now: u32) {
        set_le32(&mut self.raw, off::MTIME, now);
        set_le32(&mut self.raw, off::CTIME, now);
    }

    pub fn set_dtime(&mut self, now: u32) {
        set_le32(&mut self.raw, off::DTIME, now)
    }

    pub fn i_block(&self) -> &[u8] {
        &self.raw[I_BLOCK]
    }

    pub fn i_block_mut(&mut self) -> &mut [u8] {
        &mut self.raw[I_BLOCK]
    }

    /// The checksum seed of the blocks this inode owns (extent tree, directory blocks).
    pub fn seed(fs_seed: u32, ino: u32, generation: u32) -> u32 {
        let c = crc32c(fs_seed, &ino.to_le_bytes());
        crc32c(c, &generation.to_le_bytes())
    }

    fn checksum(&self, fs_seed: u32, ino: u32) -> u32 {
        let mut copy = self.raw;
        set_le16(&mut copy, off::CHECKSUM_LO, 0);
        set_le16(&mut copy, off::CHECKSUM_HI, 0);
        crc32c(Self::seed(fs_seed, ino, self.generation()), &copy)
    }

    pub fn set_checksum(&mut self, fs_seed: u32, ino: u32) {
        let c = self.checksum(fs_seed, ino);
        set_le16(&mut self.raw, off::CHECKSUM_LO, c as u16);
        set_le16(&mut self.raw, off::CHECKSUM_HI, (c >> 16) as u16);
    }

    pub fn checksum_ok(&self, fs_seed: u32, ino: u32) -> bool {
        let c = self.checksum(fs_seed, ino);
        le16(&self.raw, off::CHECKSUM_LO) == c as u16 && le16(&self.raw, off::CHECKSUM_HI) == (c >> 16) as u16
    }
}
