//! Making a file system: the one shape [`crate::Ext4::mount`] accepts.
//!
//! Each group starts with its superblock and descriptor copy (groups 0, 1 and powers of 3, 5
//! and 7), then its block bitmap, inode bitmap and inode table; the rest is data. Group 0's
//! data starts with the journal, then the root directory's block and `lost+found`'s.
//! Every inode table is zeroed here, so no group is left uninitialized.

use alloc::vec;
use alloc::vec::Vec;

use crate::bytes::*;
use crate::dir::{self, FT_DIR};
use crate::extent::{MAX_LEN, set_root_extents};
use crate::inode::{Inode, S_IFDIR, S_IFREG};
use crate::journal;
use crate::layout::*;
use crate::{BLOCK_SIZE, BlockDevice, Error, ROOT_INO, Result};

#[derive(Clone, Copy, Debug)]
pub struct FormatOptions {
    /// The volume's UUID (random, so two file systems never share one).
    pub uuid: [u8; 16],
    /// The label, NUL padded.
    pub label: [u8; 16],
    /// A multiple of 16; 2048 = one inode for every 64 KiB.
    pub inodes_per_group: u32,
    /// The journal's size in blocks: at least 1024, and it must fit group 0.
    pub journal_blocks: u32,
}

impl Default for FormatOptions {
    fn default() -> Self {
        Self { uuid: [0; 16], label: [0; 16], inodes_per_group: 2048, journal_blocks: 8192 }
    }
}

/// Writes zeros over blocks `start..start + count`.
fn zero<D: BlockDevice>(dev: &mut D, start: u64, count: u64, zeros: &[u8]) -> Result<()> {
    let per = (zeros.len() / BLOCK_SIZE) as u64;
    let mut b = start;
    while b < start + count {
        let n = per.min(start + count - b);
        dev.write(b, &zeros[..n as usize * BLOCK_SIZE])?;
        b += n;
    }
    Ok(())
}

fn set_bits(map: &mut [u8], from: u32, to: u32) {
    for i in from..to {
        map[(i / 8) as usize] |= 1 << (i % 8);
    }
}

/// Formats `dev` (all of it, up to the last whole group that fits its metadata).
pub fn format<D: BlockDevice>(dev: &mut D, options: &FormatOptions) -> Result<()> {
    let ipg = options.inodes_per_group;
    if ipg == 0 || ipg % INODES_PER_BLOCK != 0 || ipg > BLOCKS_PER_GROUP {
        return Err(Error::Unsupported("inodes per group"));
    }
    let mut blocks = dev.blocks().min(u32::MAX as u64);
    let mut geo = Geometry::new(blocks, ipg);
    let last = geo.groups - 1;
    if geo.group_blocks(last) < geo.overhead(last) + 64 {
        // The last group would be (nearly) all metadata: drop it.
        blocks = geo.group_start(last);
        geo = Geometry::new(blocks, ipg);
    }
    let journal_blocks = options.journal_blocks;
    let data0 = geo.overhead(0) as u64;
    let data0_end = geo.group_blocks(0) as u64;
    if geo.groups == 0 || journal_blocks < 1024 || data0 + journal_blocks as u64 + 2 + 64 > data0_end {
        return Err(Error::NoSpace);
    }
    let journal_start = data0;
    let root_block = journal_start + journal_blocks as u64;
    let lost_found_block = root_block + 1;
    let now = dev.now();

    // Superblock (the primary; copies differ in s_block_group_nr).
    let mut sb = Superblock { raw: [0; SB_SIZE] };
    let total_inodes = geo.total_inodes();
    sb.set_u32(sb::INODES_COUNT, total_inodes);
    sb.set_u32(sb::BLOCKS_COUNT, blocks as u32);
    sb.set_u32(sb::R_BLOCKS_COUNT, 0);
    sb.set_u32(sb::FIRST_DATA_BLOCK, 0);
    sb.set_u32(sb::LOG_BLOCK_SIZE, 2);
    sb.set_u32(sb::LOG_CLUSTER_SIZE, 2);
    sb.set_u32(sb::BLOCKS_PER_GROUP, BLOCKS_PER_GROUP);
    sb.set_u32(sb::CLUSTERS_PER_GROUP, BLOCKS_PER_GROUP);
    sb.set_u32(sb::INODES_PER_GROUP, ipg);
    sb.set_u32(sb::WTIME, now);
    sb.set_u16(sb::MAX_MNT_COUNT, 0xFFFF);
    sb.set_u16(sb::MAGIC, MAGIC);
    sb.set_u16(sb::STATE, 1);
    sb.set_u16(sb::ERRORS, 1);
    sb.set_u32(sb::LASTCHECK, now);
    sb.set_u32(sb::REV_LEVEL, 1);
    sb.set_u32(sb::FIRST_INO, FIRST_INO);
    sb.set_u16(sb::INODE_SIZE, INODE_SIZE as u16);
    sb.set_u32(sb::FEATURE_COMPAT, COMPAT);
    sb.set_u32(sb::FEATURE_INCOMPAT, INCOMPAT);
    sb.set_u32(sb::FEATURE_RO_COMPAT, RO_COMPAT);
    sb.raw[sb::UUID..sb::UUID + 16].copy_from_slice(&options.uuid);
    sb.raw[sb::VOLUME_NAME..sb::VOLUME_NAME + 16].copy_from_slice(&options.label);
    sb.set_u32(sb::JOURNAL_INUM, JOURNAL_INO);
    for i in 0..4 {
        let word = le32(&options.uuid, ((i + 1) % 4) * 4).rotate_left(7 * i as u32 + 3) ^ 0x5A5A_A5A5;
        sb.set_u32(sb::HASH_SEED + i * 4, word);
    }
    sb.raw[sb::DEF_HASH_VERSION] = 1;
    sb.raw[sb::JNL_BACKUP_TYPE] = 1;
    sb.set_u32(sb::MKFS_TIME, now);
    sb.set_u16(sb::MIN_EXTRA_ISIZE, EXTRA_ISIZE);
    sb.set_u16(sb::WANT_EXTRA_ISIZE, EXTRA_ISIZE);
    sb.set_u32(sb::FLAGS, 0x2); // unsigned directory hash
    sb.raw[sb::CHECKSUM_TYPE] = 1;
    let seed = sb.csum_seed();

    // Inodes: root, journal, lost+found.
    let mut root = Inode::new(S_IFDIR | 0o755, now, 0);
    root.set_links(3);
    root.set_size(BLOCK_SIZE as u64);
    root.add_blocks(1);
    set_root_extents(&mut root, &[(0, root_block, 1)]);
    let mut lost_found = Inode::new(S_IFDIR | 0o700, now, 0);
    lost_found.set_links(2);
    lost_found.set_size(BLOCK_SIZE as u64);
    lost_found.add_blocks(1);
    set_root_extents(&mut lost_found, &[(0, lost_found_block, 1)]);
    let mut jinode = Inode::new(S_IFREG | 0o600, now, 0);
    jinode.set_size(journal_blocks as u64 * BLOCK_SIZE as u64);
    jinode.add_blocks(journal_blocks as i64);
    let mut list: Vec<(u32, u64, u32)> = Vec::new();
    let mut l = 0;
    while l < journal_blocks {
        let n = (journal_blocks - l).min(MAX_LEN);
        list.push((l, journal_start + l as u64, n));
        l += n;
    }
    if list.len() > 4 {
        return Err(Error::Unsupported("journal too large"));
    }
    set_root_extents(&mut jinode, &list);
    for i in 0..15 {
        sb.set_u32(sb::JNL_BLOCKS + i * 4, le32(jinode.i_block(), i * 4));
    }
    sb.set_u32(sb::JNL_BLOCKS + 15 * 4, (jinode.size() >> 32) as u32);
    sb.set_u32(sb::JNL_BLOCKS + 16 * 4, jinode.size() as u32);

    // Bitmaps and descriptors.
    let zeros = vec![0u8; 64 * BLOCK_SIZE];
    let mut descs: Vec<GroupDesc> = Vec::with_capacity(geo.groups as usize);
    let mut free_blocks_total = 0u64;
    let mut bmap = vec![0u8; BLOCK_SIZE];
    let mut imap = vec![0u8; BLOCK_SIZE];
    for g in 0..geo.groups {
        let count = geo.group_blocks(g);
        bmap.fill(0);
        set_bits(&mut bmap, 0, geo.overhead(g));
        if g == 0 {
            set_bits(&mut bmap, journal_start as u32, lost_found_block as u32 + 1);
        }
        set_bits(&mut bmap, count, BLOCKS_PER_GROUP);
        imap.fill(0);
        if g == 0 {
            set_bits(&mut imap, 0, FIRST_INO);
        }
        set_bits(&mut imap, ipg, (BLOCK_SIZE * 8) as u32);

        let used_blocks: u32 = (0..count).filter(|&i| bmap[(i / 8) as usize] & (1 << (i % 8)) != 0).count() as u32;
        let free_blocks = count - used_blocks;
        free_blocks_total += free_blocks as u64;
        let used_inodes = if g == 0 { FIRST_INO } else { 0 };

        let bb = geo.block_bitmap(g);
        let mut d = GroupDesc { raw: [0; DESC_SIZE] };
        d.set_u32(gd::BLOCK_BITMAP, bb as u32);
        d.set_u32(gd::INODE_BITMAP, bb as u32 + 1);
        d.set_u32(gd::INODE_TABLE, bb as u32 + 2);
        d.set_u16(gd::FREE_BLOCKS, free_blocks as u16);
        d.set_u16(gd::FREE_INODES, (ipg - used_inodes) as u16);
        d.set_u16(gd::USED_DIRS, if g == 0 { 2 } else { 0 });
        d.set_u16(gd::FLAGS, BG_INODE_ZEROED);
        d.set_u16(gd::BLOCK_BITMAP_CSUM, bitmap_checksum(seed, &bmap, BLOCKS_PER_GROUP));
        d.set_u16(gd::INODE_BITMAP_CSUM, bitmap_checksum(seed, &imap, ipg));
        d.set_u16(gd::ITABLE_UNUSED, (ipg - used_inodes) as u16);
        d.set_checksum(seed, g);
        descs.push(d);

        dev.write(bb, &bmap)?;
        dev.write(bb + 1, &imap)?;
        zero(dev, bb + 2, geo.itable_blocks as u64, &zeros)?;
    }
    sb.set_free_blocks(free_blocks_total);
    sb.set_free_inodes(total_inodes - FIRST_INO);

    // The first inode table block of group 0 holds inodes 1–16.
    let itable = geo.block_bitmap(0) + 2;
    let mut iblock = vec![0u8; BLOCK_SIZE];
    for (ino, inode) in [(ROOT_INO, &mut root), (JOURNAL_INO, &mut jinode), (LOST_FOUND_INO, &mut lost_found)] {
        inode.set_checksum(seed, ino);
        let at = (ino - 1) as usize * INODE_SIZE;
        iblock[at..at + INODE_SIZE].copy_from_slice(&inode.raw);
    }
    dev.write(itable, &iblock)?;

    // Directory blocks.
    let mut b = vec![0u8; BLOCK_SIZE];
    let root_seed = Inode::seed(seed, ROOT_INO, 0);
    dir::init_block(&mut b, ROOT_INO, ROOT_INO, root_seed);
    // "lost+found" in the root, after "..": shrink ".." to its size.
    set_le16(&mut b, 12 + 4, 12);
    let name = b"lost+found";
    set_le32(&mut b, 24, LOST_FOUND_INO);
    set_le16(&mut b, 28, (BLOCK_SIZE - 12 - 24) as u16);
    b[30] = name.len() as u8;
    b[31] = FT_DIR;
    b[32..32 + name.len()].copy_from_slice(name);
    let c = crate::crc32c::crc32c(root_seed, &b[..BLOCK_SIZE - 12]);
    set_le32(&mut b, BLOCK_SIZE - 4, c);
    dev.write(root_block, &b)?;
    b.fill(0);
    dir::init_block(&mut b, LOST_FOUND_INO, ROOT_INO, Inode::seed(seed, LOST_FOUND_INO, 0));
    dev.write(lost_found_block, &b)?;

    // The journal: zeroed, then its superblock.
    zero(dev, journal_start, journal_blocks as u64, &zeros)?;
    dev.write(journal_start, &journal::new_superblock(journal_blocks, &options.uuid))?;

    // Superblock and descriptor copies: the primary last.
    let mut gdt = vec![0u8; geo.gdt_blocks as usize * BLOCK_SIZE];
    for (g, d) in descs.iter().enumerate() {
        gdt[g * DESC_SIZE..(g + 1) * DESC_SIZE].copy_from_slice(&d.raw);
    }
    for g in (0..geo.groups).rev().filter(|&g| has_super(g)) {
        let start = geo.group_start(g);
        let mut copy = sb.clone();
        copy.set_u16(sb::BLOCK_GROUP_NR, g as u16);
        copy.set_checksum();
        b.fill(0);
        let at = if g == 0 { SB_OFFSET } else { 0 };
        b[at..at + SB_SIZE].copy_from_slice(&copy.raw);
        dev.write(start, &b)?;
        dev.write(start + 1, &gdt)?;
    }
    dev.flush()?;
    Ok(())
}
