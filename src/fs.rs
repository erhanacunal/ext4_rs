//! The mounted file system and its operations.

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::vec;
use alloc::vec::Vec;

use crate::cache::Cache;
use crate::dir::{self, FT_DIR, FT_REG};
use crate::extent::Mapping;
use crate::inode::{INDEX_FL, Inode, S_IFDIR, S_IFREG};
use crate::journal::Journal;
use crate::layout::*;
use crate::{BLOCK_SIZE, BlockDevice, Error, ROOT_INO, Result};

/// An inode number.
pub type Ino = u32;

/// What an inode is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    File,
    Dir,
    /// Anything else (this crate makes none).
    Other,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Metadata {
    pub kind: Kind,
    pub size: u64,
    /// Seconds since 1970.
    pub mtime: u32,
}

/// One entry [`Ext4::read_dir`] hands out (`.` and `..` left out).
pub struct DirEntry<'a> {
    pub name: &'a [u8],
    pub ino: Ino,
    pub kind: Kind,
}

#[derive(Clone, Copy, Debug)]
pub struct Stats {
    pub blocks: u64,
    pub free_blocks: u64,
    pub inodes: u32,
    pub free_inodes: u32,
}

#[derive(Clone, Copy, Debug)]
pub struct MountOptions {
    /// Clean metadata blocks the cache keeps (4 KiB each).
    pub cache_blocks: usize,
    /// A transaction commits by itself once it holds this many blocks (4 KiB each, kept in
    /// memory until then). Capped by the journal's size.
    pub commit_blocks: usize,
}

impl Default for MountOptions {
    fn default() -> Self {
        Self { cache_blocks: 1024, commit_blocks: 1024 }
    }
}

/// Data blocks one step of [`Ext4::write_at`] allocates or writes (a transaction may commit
/// between steps).
const WRITE_STEP: u32 = 1024;

pub struct Ext4<D: BlockDevice> {
    pub(crate) dev: D,
    pub(crate) cache: Cache,
    pub(crate) sb: Superblock,
    pub(crate) sb_dirty: bool,
    pub(crate) geo: Geometry,
    pub(crate) groups: Vec<GroupDesc>,
    /// Groups whose descriptor or bitmaps changed in the running transaction.
    pub(crate) dirty_groups: BTreeSet<u32>,
    pub(crate) seed: u32,
    pub(crate) journal: Journal,
    /// Blocks freed in the running transaction (start → length): not given out again until
    /// it commits, so a crash before then cannot leave a file pointing at reused blocks.
    pub(crate) freed: BTreeMap<u64, u32>,
    /// A write failed: no more changes (the journal keeps the device consistent).
    pub(crate) failed: bool,
    /// [`Cache::changes`] when the last commit finished.
    pub(crate) committed_at: u64,
    pub(crate) commit_blocks: usize,
    pub(crate) generation: u32,
    /// Whether mount found the file system not cleanly unmounted, and replayed a transaction.
    pub(crate) unclean: bool,
    pub(crate) replayed: bool,
}

/// Checks one path component.
fn check_name(name: &[u8]) -> Result<()> {
    if name.is_empty() || name.len() > 255 || name == b"." || name == b".." || name.contains(&0) || name.contains(&b'/') {
        return Err(Error::BadName);
    }
    Ok(())
}

fn components(path: &str) -> impl Iterator<Item = &[u8]> {
    path.split('/').filter(|c| !c.is_empty()).map(str::as_bytes)
}

impl<D: BlockDevice> Ext4<D> {
    /// Mounts the file system on `dev`, replaying a committed transaction a power loss left
    /// in the journal.
    pub fn mount(mut dev: D, options: MountOptions) -> Result<Self> {
        let mut block0 = vec![0u8; BLOCK_SIZE];
        dev.read(0, &mut block0)?;
        let mut sb = Superblock { raw: block0[SB_OFFSET..SB_OFFSET + SB_SIZE].try_into().unwrap() };
        let geo = sb.check(dev.blocks())?;
        let seed = sb.csum_seed();
        let mut cache = Cache::new(options.cache_blocks);

        // The journal, before anything else is read: a replay may rewrite any block.
        let journal_inode = read_inode_raw(&mut dev, &mut cache, &geo, seed, JOURNAL_INO)?;
        let unclean = sb.u32(sb::FEATURE_INCOMPAT) & INCOMPAT_RECOVER != 0;
        let mut journal = Journal::open(&mut dev, &journal_inode, &sb.uuid())?;
        let replayed = journal.replay(&mut dev)?;
        if replayed {
            cache.clear();
            dev.read(0, &mut block0)?;
            sb = Superblock { raw: block0[SB_OFFSET..SB_OFFSET + SB_SIZE].try_into().unwrap() };
            sb.check(dev.blocks())?;
        }

        let mut groups = Vec::with_capacity(geo.groups as usize);
        for g in 0..geo.groups {
            let block = cache.get(&mut dev, 1 + (g / DESCS_PER_BLOCK) as u64)?;
            let at = (g % DESCS_PER_BLOCK) as usize * DESC_SIZE;
            let desc = GroupDesc { raw: block[at..at + DESC_SIZE].try_into().unwrap() };
            if desc.u16(gd::CHECKSUM) != desc.checksum(seed, g) {
                return Err(Error::Corrupt("group descriptor checksum"));
            }
            let bb = geo.block_bitmap(g);
            if desc.u32(gd::BLOCK_BITMAP) as u64 != bb
                || desc.u32(gd::INODE_BITMAP) as u64 != bb + 1
                || desc.u32(gd::INODE_TABLE) as u64 != bb + 2
                || desc.u16(gd::FLAGS) & !(BG_INODE_ZEROED | BG_INODE_UNINIT | BG_BLOCK_UNINIT) != 0
            {
                return Err(Error::Unsupported("group layout"));
            }
            groups.push(desc);
        }

        let commit_blocks = options.commit_blocks.clamp(16, journal.capacity());
        let now = dev.now();
        let mut fs = Self {
            generation: sb.u32(sb::WTIME) ^ now.rotate_left(16),
            dev,
            cache,
            sb,
            sb_dirty: false,
            geo,
            groups,
            dirty_groups: BTreeSet::new(),
            seed,
            journal,
            freed: BTreeMap::new(),
            failed: false,
            committed_at: 0,
            commit_blocks,
            unclean,
            replayed,
        };
        fs.init_uninit_groups();
        // Mark it in use (as Linux does): needs_recovery until a clean unmount.
        let incompat = fs.sb.u32(sb::FEATURE_INCOMPAT);
        fs.sb.set_u32(sb::FEATURE_INCOMPAT, incompat | INCOMPAT_RECOVER);
        fs.sb.set_u32(sb::MTIME, now);
        let count = fs.sb.u16(sb::MNT_COUNT);
        fs.sb.set_u16(sb::MNT_COUNT, count.wrapping_add(1));
        fs.write_sb_direct()?;
        Ok(fs)
    }

    /// Groups e2fsck (or Linux) left uninitialized get their bitmaps built in the cache (dirty,
    /// so the next commit writes them) and lose the flag.
    fn init_uninit_groups(&mut self) {
        for g in 0..self.geo.groups {
            let flags = self.groups[g as usize].u16(gd::FLAGS);
            if flags & (BG_INODE_UNINIT | BG_BLOCK_UNINIT) == 0 {
                continue;
            }
            let bb = self.geo.block_bitmap(g);
            if flags & BG_BLOCK_UNINIT != 0 {
                let (used, count) = (self.geo.overhead(g), self.geo.group_blocks(g));
                let map = self.cache.zeroed(bb);
                for i in (0..used).chain(count..BLOCKS_PER_GROUP) {
                    map[(i / 8) as usize] |= 1 << (i % 8);
                }
            }
            if flags & BG_INODE_UNINIT != 0 {
                let map = self.cache.zeroed(bb + 1);
                for i in self.geo.inodes_per_group..(BLOCK_SIZE * 8) as u32 {
                    map[(i / 8) as usize] |= 1 << (i % 8);
                }
            }
            self.groups[g as usize].set_u16(gd::FLAGS, flags & !(BG_INODE_UNINIT | BG_BLOCK_UNINIT));
            self.dirty_groups.insert(g);
        }
    }

    /// Commits what is pending, marks the file system clean and hands the device back.
    pub fn unmount(mut self) -> Result<D> {
        self.commit()?;
        let incompat = self.sb.u32(sb::FEATURE_INCOMPAT);
        self.sb.set_u32(sb::FEATURE_INCOMPAT, incompat & !INCOMPAT_RECOVER);
        self.write_sb_direct()?;
        Ok(self.dev)
    }

    /// The device, for callers that keep other data on it.
    pub fn device(&mut self) -> &mut D {
        &mut self.dev
    }

    /// Writes the superblock to block 0 outside the journal (mount and unmount only, when the
    /// log is empty).
    fn write_sb_direct(&mut self) -> Result<()> {
        let now = self.dev.now();
        self.sb.set_u32(sb::WTIME, now);
        self.sb.set_checksum();
        let mut block = vec![0u8; BLOCK_SIZE];
        self.dev.read(0, &mut block)?;
        block[SB_OFFSET..SB_OFFSET + SB_SIZE].copy_from_slice(&self.sb.raw);
        self.dev.write(0, &block)?;
        self.dev.flush()?;
        self.cache.invalidate(0);
        Ok(())
    }

    /// What mount found: (not cleanly unmounted, a committed transaction replayed).
    pub fn mount_state(&self) -> (bool, bool) {
        (self.unclean, self.replayed)
    }

    pub fn stats(&self) -> Stats {
        Stats {
            blocks: self.geo.blocks,
            free_blocks: self.sb.free_blocks(),
            inodes: self.geo.total_inodes(),
            free_inodes: self.sb.free_inodes(),
        }
    }

    pub(crate) fn now(&self) -> u32 {
        self.dev.now()
    }

    /// Fails changes once a write failed.
    pub(crate) fn writable(&self) -> Result<()> {
        if self.failed { Err(Error::ReadOnly) } else { Ok(()) }
    }

    /// Commits the running transaction once it is large.
    pub(crate) fn maybe_commit(&mut self) -> Result<()> {
        if self.cache.dirty_count() + self.dirty_groups.len() + 2 >= self.commit_blocks {
            self.commit()?;
        }
        Ok(())
    }

    /// Commits the running transaction and makes everything written so far durable.
    pub fn sync(&mut self) -> Result<()> {
        self.writable()?;
        self.commit()
    }

    /// Runs a change. An error after it changed the running transaction makes the file
    /// system read-only: the half-done transaction never commits, and the journal keeps the
    /// device as of the last commit.
    fn change<T>(&mut self, f: impl FnOnce(&mut Self) -> Result<T>) -> Result<T> {
        self.writable()?;
        let before = self.cache.changes;
        let result = f(self);
        if result.is_err() && self.cache.changes != before.max(self.committed_at) {
            self.failed = true;
            self.cache.discard_dirty();
        }
        result
    }

    /// Creates an empty file; [`Error::Exists`] when the name is taken.
    pub fn create(&mut self, path: &str) -> Result<Ino> {
        self.change(|fs| fs.create_inner(path))
    }

    /// Creates an empty directory; [`Error::Exists`] when the name is taken.
    pub fn mkdir(&mut self, path: &str) -> Result<Ino> {
        self.change(|fs| fs.mkdir_inner(path))
    }

    /// Deletes a file or an empty directory.
    pub fn remove(&mut self, path: &str) -> Result<()> {
        self.change(|fs| fs.remove_inner(path))
    }

    /// Renames `from` to `to`, replacing a file (or an empty directory, when `from` is one)
    /// that `to` names. One transaction: after a crash either name holds it, never both or
    /// none.
    pub fn rename(&mut self, from: &str, to: &str) -> Result<()> {
        self.change(|fs| fs.rename_inner(from, to))
    }

    /// Writes `data` at `offset` (past the end leaves a hole that reads as zeros). Returns the
    /// bytes written: all of them, or fewer when the device fills up.
    pub fn write_at(&mut self, ino: Ino, offset: u64, data: &[u8]) -> Result<usize> {
        self.change(|fs| fs.write_at_inner(ino, offset, data))
    }

    /// Sets the file's length: shorter frees the blocks past it, longer leaves a hole.
    pub fn set_len(&mut self, ino: Ino, len: u64) -> Result<()> {
        self.change(|fs| fs.set_len_inner(ino, len))
    }

    // ---- inodes ----

    pub(crate) fn inode_location(&self, ino: Ino) -> Result<(u64, usize)> {
        if ino == 0 || ino > self.geo.total_inodes() {
            return Err(Error::Corrupt("inode number"));
        }
        let g = (ino - 1) / self.geo.inodes_per_group;
        let index = (ino - 1) % self.geo.inodes_per_group;
        let block = self.geo.block_bitmap(g) + 2 + (index / INODES_PER_BLOCK) as u64;
        Ok((block, (index % INODES_PER_BLOCK) as usize * INODE_SIZE))
    }

    pub(crate) fn read_inode(&mut self, ino: Ino) -> Result<Inode> {
        read_inode_raw(&mut self.dev, &mut self.cache, &self.geo, self.seed, ino)
    }

    pub(crate) fn write_inode(&mut self, ino: Ino, inode: &mut Inode) -> Result<()> {
        inode.set_checksum(self.seed, ino);
        let (block, at) = self.inode_location(ino)?;
        let data = self.cache.get_mut(&mut self.dev, block)?;
        data[at..at + INODE_SIZE].copy_from_slice(&inode.raw);
        Ok(())
    }

    pub(crate) fn next_generation(&mut self) -> u32 {
        self.generation = self.generation.wrapping_mul(1_103_515_245).wrapping_add(12345);
        self.generation
    }

    pub(crate) fn group_of_ino(&self, ino: Ino) -> u32 {
        (ino - 1) / self.geo.inodes_per_group
    }

    // ---- paths ----

    /// The inode `path` names, which must exist.
    fn resolve(&mut self, path: &str) -> Result<Ino> {
        let mut ino = ROOT_INO;
        for name in components(path) {
            check_name(name)?;
            let inode = self.read_inode(ino)?;
            if !inode.is_dir() {
                return Err(Error::NotDir);
            }
            ino = self.dir_lookup(ino, &inode, name)?.ok_or(Error::NotFound)?.0;
        }
        Ok(ino)
    }

    /// `path`'s parent directory (which must exist) and last component.
    fn resolve_parent<'p>(&mut self, path: &'p str) -> Result<(Ino, Inode, &'p [u8])> {
        let path = path.trim_end_matches('/');
        let (parent, name) = match path.rfind('/') {
            Some(i) => (&path[..i], &path[i + 1..]),
            None => ("", path),
        };
        let name = name.as_bytes();
        check_name(name)?;
        let ino = self.resolve(parent)?;
        let inode = self.read_inode(ino)?;
        if !inode.is_dir() {
            return Err(Error::NotDir);
        }
        if inode.flags() & INDEX_FL != 0 {
            return Err(Error::Unsupported("hashed directory"));
        }
        Ok((ino, inode, name))
    }

    /// The inode at `path`; `Ok(None)` when its last component does not exist (a missing
    /// directory on the way is [`Error::NotFound`]).
    pub fn lookup(&mut self, path: &str) -> Result<Option<Ino>> {
        if components(path).next().is_none() {
            return Ok(Some(ROOT_INO));
        }
        let (dir, inode, name) = self.resolve_parent(path)?;
        Ok(self.dir_lookup(dir, &inode, name)?.map(|(ino, _)| ino))
    }

    pub fn metadata(&mut self, ino: Ino) -> Result<Metadata> {
        let inode = self.read_inode(ino)?;
        let kind = if inode.is_dir() {
            Kind::Dir
        } else if inode.is_file() {
            Kind::File
        } else {
            Kind::Other
        };
        Ok(Metadata { kind, size: inode.size(), mtime: inode.mtime() })
    }

    /// Calls `f` for each entry of the directory `ino` (`.` and `..` left out) until it
    /// returns `false`.
    pub fn read_dir(&mut self, ino: Ino, mut f: impl FnMut(&DirEntry) -> bool) -> Result<()> {
        let inode = self.read_inode(ino)?;
        if !inode.is_dir() {
            return Err(Error::NotDir);
        }
        self.dir_for_each(ino, &inode, |name, ino, ft| {
            if name == b"." || name == b".." {
                return true;
            }
            let kind = match ft {
                FT_REG => Kind::File,
                FT_DIR => Kind::Dir,
                _ => Kind::Other,
            };
            f(&DirEntry { name, ino, kind })
        })
    }

    // ---- creating and removing ----

    fn create_inner(&mut self, path: &str) -> Result<Ino> {
        self.writable()?;
        self.maybe_commit()?;
        let (dir, mut dir_inode, name) = self.resolve_parent(path)?;
        if self.dir_lookup(dir, &dir_inode, name)?.is_some() {
            return Err(Error::Exists);
        }
        let ino = self.alloc_inode(false, self.group_of_ino(dir))?;
        let now = self.now();
        let generation = self.next_generation();
        let mut inode = Inode::new(S_IFREG | 0o644, now, generation);
        self.write_inode(ino, &mut inode)?;
        self.dir_add(dir, &mut dir_inode, name, ino, FT_REG)?;
        dir_inode.touch(now);
        self.write_inode(dir, &mut dir_inode)?;
        Ok(ino)
    }

    fn mkdir_inner(&mut self, path: &str) -> Result<Ino> {
        self.writable()?;
        self.maybe_commit()?;
        let (dir, mut dir_inode, name) = self.resolve_parent(path)?;
        if self.dir_lookup(dir, &dir_inode, name)?.is_some() {
            return Err(Error::Exists);
        }
        if dir_inode.links() >= 65000 {
            return Err(Error::NoSpace);
        }
        let ino = self.alloc_inode(true, self.group_of_ino(dir))?;
        let now = self.now();
        let generation = self.next_generation();
        let mut inode = Inode::new(S_IFDIR | 0o755, now, generation);
        inode.set_links(2);
        let goal = self.geo.group_start(self.group_of_ino(ino));
        let (block, _) = match self.alloc_blocks(goal, 1) {
            Ok(b) => b,
            Err(e) => {
                self.free_inode(ino, true)?;
                return Err(e);
            }
        };
        let seed = Inode::seed(self.seed, ino, generation);
        dir::init_block(self.cache.zeroed(block), ino, dir, seed);
        self.extent_insert(ino, &mut inode, 0, block, 1)?;
        inode.add_blocks(1);
        inode.set_size(BLOCK_SIZE as u64);
        self.write_inode(ino, &mut inode)?;
        self.dir_add(dir, &mut dir_inode, name, ino, FT_DIR)?;
        dir_inode.set_links(dir_inode.links() + 1);
        dir_inode.touch(now);
        self.write_inode(dir, &mut dir_inode)?;
        Ok(ino)
    }

    /// Frees an inode and everything it holds (its last link is gone).
    fn release(&mut self, ino: Ino, mut inode: Inode) -> Result<()> {
        self.extent_truncate(ino, &mut inode, 0)?;
        let dir = inode.is_dir();
        let now = self.now();
        inode.set_links(0);
        inode.set_size(0);
        inode.set_dtime(now);
        self.write_inode(ino, &mut inode)?;
        self.free_inode(ino, dir)
    }

    fn remove_inner(&mut self, path: &str) -> Result<()> {
        self.writable()?;
        self.maybe_commit()?;
        let (dir, mut dir_inode, name) = self.resolve_parent(path)?;
        let (ino, _) = self.dir_lookup(dir, &dir_inode, name)?.ok_or(Error::NotFound)?;
        let inode = self.read_inode(ino)?;
        if inode.is_dir() && !self.dir_is_empty(ino, &inode)? {
            return Err(Error::NotEmpty);
        }
        self.dir_remove(dir, &dir_inode, name)?;
        let now = self.now();
        if inode.is_dir() {
            dir_inode.set_links(dir_inode.links() - 1);
        }
        dir_inode.touch(now);
        self.write_inode(dir, &mut dir_inode)?;
        self.release(ino, inode)
    }

    fn rename_inner(&mut self, from: &str, to: &str) -> Result<()> {
        self.writable()?;
        self.maybe_commit()?;
        let (from_dir, from_dir_inode, from_name) = self.resolve_parent(from)?;
        let (ino, ft) = self.dir_lookup(from_dir, &from_dir_inode, from_name)?.ok_or(Error::NotFound)?;
        let (to_dir, to_dir_inode, to_name) = self.resolve_parent(to)?;
        if from_dir == to_dir && from_name == to_name {
            return Ok(());
        }
        let inode = self.read_inode(ino)?;
        let moving_dir = inode.is_dir();
        if moving_dir && from_dir != to_dir {
            // Not into itself: walk up from the new parent.
            let mut up = to_dir;
            while up != ROOT_INO {
                if up == ino {
                    return Err(Error::BadName);
                }
                let up_inode = self.read_inode(up)?;
                up = self.dir_lookup(up, &up_inode, b"..")?.ok_or(Error::Corrupt("no .."))?.0;
            }
        }
        let replaced = self.dir_lookup(to_dir, &to_dir_inode, to_name)?;
        if let Some((old, _)) = replaced {
            let old_inode = self.read_inode(old)?;
            match (moving_dir, old_inode.is_dir()) {
                (false, true) => return Err(Error::IsDir),
                (true, false) => return Err(Error::NotDir),
                (true, true) if !self.dir_is_empty(old, &old_inode)? => return Err(Error::NotEmpty),
                _ => {}
            }
        }
        let now = self.now();
        // The new name first, then the old one goes; the parents' inodes are re-read as each
        // step changes them.
        let mut to_dir_inode = to_dir_inode;
        if let Some((old, _)) = replaced {
            self.dir_set_entry(to_dir, &to_dir_inode, to_name, ino, ft)?;
            let old_inode = self.read_inode(old)?;
            if old_inode.is_dir() {
                to_dir_inode.set_links(to_dir_inode.links() - 1);
            }
            self.release(old, old_inode)?;
        } else {
            self.dir_add(to_dir, &mut to_dir_inode, to_name, ino, ft)?;
        }
        if moving_dir && from_dir != to_dir {
            to_dir_inode.set_links(to_dir_inode.links() + 1);
        }
        to_dir_inode.touch(now);
        self.write_inode(to_dir, &mut to_dir_inode)?;

        let mut from_dir_inode = self.read_inode(from_dir)?;
        self.dir_remove(from_dir, &from_dir_inode, from_name)?;
        if moving_dir && from_dir != to_dir {
            from_dir_inode.set_links(from_dir_inode.links() - 1);
            let moved = self.read_inode(ino)?;
            self.dir_set_entry(ino, &moved, b"..", to_dir, FT_DIR)?;
        }
        from_dir_inode.touch(now);
        self.write_inode(from_dir, &mut from_dir_inode)?;
        Ok(())
    }

    // ---- file contents ----

    fn file_inode(&mut self, ino: Ino) -> Result<Inode> {
        let inode = self.read_inode(ino)?;
        if inode.is_dir() {
            return Err(Error::IsDir);
        }
        if !inode.is_file() {
            return Err(Error::Unsupported("not a regular file"));
        }
        Ok(inode)
    }

    /// Reads from `offset`; returns the bytes read (fewer at the end of the file).
    pub fn read_at(&mut self, ino: Ino, offset: u64, buf: &mut [u8]) -> Result<usize> {
        let inode = self.file_inode(ino)?;
        let size = inode.size();
        if offset >= size {
            return Ok(0);
        }
        let len = buf.len().min((size - offset) as usize);
        let bs = BLOCK_SIZE as u64;
        let mut done = 0usize;
        let mut tmp: Option<Vec<u8>> = None;
        while done < len {
            let pos = offset + done as u64;
            let lblk = (pos / bs) as u32;
            let in_block = (pos % bs) as usize;
            let mapping = self.extent_map(ino, &inode, lblk)?;
            let run = match mapping {
                Mapping::Mapped(_, run) | Mapping::Hole(run) => run as u64,
            };
            // Bytes this mapping covers from `pos`.
            let span = (run * bs - in_block as u64).min((len - done) as u64) as usize;
            let out = &mut buf[done..done + span];
            match mapping {
                Mapping::Hole(_) => out.fill(0),
                Mapping::Mapped(pblk, _) => {
                    let head = if in_block != 0 { (BLOCK_SIZE - in_block).min(span) } else { 0 };
                    let whole = (span - head) / BLOCK_SIZE;
                    let tail = span - head - whole * BLOCK_SIZE;
                    let tmp = tmp.get_or_insert_with(|| vec![0; BLOCK_SIZE]);
                    let mut p = pblk;
                    if head > 0 {
                        self.dev.read(p, tmp)?;
                        out[..head].copy_from_slice(&tmp[in_block..in_block + head]);
                        p += 1;
                    }
                    if whole > 0 {
                        self.dev.read(p, &mut out[head..head + whole * BLOCK_SIZE])?;
                        p += whole as u64;
                    }
                    if tail > 0 {
                        self.dev.read(p, tmp)?;
                        out[span - tail..].copy_from_slice(&tmp[..tail]);
                    }
                }
            }
            done += span;
        }
        Ok(len)
    }

    /// Where a new block for logical block `lblk` of `ino` should go: after the block before
    /// it, else at the start of the inode's group.
    fn data_goal(&mut self, ino: Ino, inode: &Inode, lblk: u32) -> Result<u64> {
        if lblk > 0 {
            if let Mapping::Mapped(p, _) = self.extent_map(ino, inode, lblk - 1)? {
                return Ok(p + 1);
            }
        }
        let g = self.group_of_ino(ino);
        Ok(self.geo.group_start(g) + self.geo.overhead(g) as u64)
    }

    fn write_at_inner(&mut self, ino: Ino, offset: u64, data: &[u8]) -> Result<usize> {
        self.writable()?;
        let bs = BLOCK_SIZE as u64;
        let end = offset.checked_add(data.len() as u64).ok_or(Error::TooLarge)?;
        if end.div_ceil(bs) > u32::MAX as u64 {
            return Err(Error::TooLarge);
        }
        let mut done = 0usize;
        let mut tmp = vec![0u8; BLOCK_SIZE];
        while done < data.len() {
            self.maybe_commit()?;
            let mut inode = self.file_inode(ino)?;
            let pos = offset + done as u64;
            let lblk = (pos / bs) as u32;
            let in_block = (pos % bs) as usize;
            let left = data.len() - done;
            let blocks_left = ((in_block + left) as u64).div_ceil(bs) as u32;
            let want = blocks_left.min(WRITE_STEP);
            let (pblk, run, fresh) = match self.extent_map(ino, &inode, lblk)? {
                Mapping::Mapped(p, run) => (p, run.min(want), false),
                Mapping::Hole(hole) => {
                    let goal = self.data_goal(ino, &inode, lblk)?;
                    match self.alloc_blocks(goal, hole.min(want)) {
                        Ok((p, n)) => (p, n, true),
                        Err(Error::NoSpace) if done > 0 => break,
                        Err(e) => return Err(e),
                    }
                }
            };
            let span = ((run as u64 * bs) as usize - in_block).min(left);
            let src = &data[done..done + span];
            if let Err(e) = self.write_blocks(pblk, in_block, src, fresh, &mut tmp) {
                self.failed = true;
                return Err(e);
            }
            if fresh {
                if let Err(e) = self.extent_insert(ino, &mut inode, lblk, pblk, run) {
                    self.free_blocks(pblk, run)?;
                    if e == Error::NoSpace && done > 0 {
                        break;
                    }
                    return Err(e);
                }
                inode.add_blocks(run as i64);
            }
            done += span;
            let now = self.now();
            inode.touch(now);
            if pos + span as u64 > inode.size() {
                inode.set_size(pos + span as u64);
            }
            self.write_inode(ino, &mut inode)?;
        }
        Ok(done)
    }

    /// Writes `src` into the blocks from `pblk` on, starting `in_block` bytes into the first.
    /// `fresh` blocks were just allocated: their bytes outside `src` become zeros; otherwise
    /// they are kept (read first).
    fn write_blocks(&mut self, pblk: u64, in_block: usize, src: &[u8], fresh: bool, tmp: &mut [u8]) -> Result<()> {
        let head = if in_block != 0 { (BLOCK_SIZE - in_block).min(src.len()) } else { 0 };
        let whole = (src.len() - head) / BLOCK_SIZE;
        let tail = src.len() - head - whole * BLOCK_SIZE;
        let mut p = pblk;
        if head > 0 {
            if fresh { tmp.fill(0) } else { self.dev.read(p, tmp)? }
            tmp[in_block..in_block + head].copy_from_slice(&src[..head]);
            self.dev.write(p, tmp)?;
            p += 1;
        }
        if whole > 0 {
            self.dev.write(p, &src[head..head + whole * BLOCK_SIZE])?;
            p += whole as u64;
        }
        if tail > 0 {
            if fresh { tmp.fill(0) } else { self.dev.read(p, tmp)? }
            tmp[..tail].copy_from_slice(&src[src.len() - tail..]);
            self.dev.write(p, tmp)?;
        }
        Ok(())
    }

    fn set_len_inner(&mut self, ino: Ino, len: u64) -> Result<()> {
        self.writable()?;
        self.maybe_commit()?;
        let mut inode = self.file_inode(ino)?;
        let bs = BLOCK_SIZE as u64;
        if len.div_ceil(bs) > u32::MAX as u64 {
            return Err(Error::TooLarge);
        }
        if len < inode.size() {
            self.extent_truncate(ino, &mut inode, len.div_ceil(bs) as u32)?;
            // The rest of the last block becomes zeros, as bytes past the end always are.
            let in_block = (len % bs) as usize;
            if in_block != 0 {
                if let Mapping::Mapped(p, _) = self.extent_map(ino, &inode, (len / bs) as u32)? {
                    let mut tmp = vec![0u8; BLOCK_SIZE];
                    self.dev.read(p, &mut tmp)?;
                    tmp[in_block..].fill(0);
                    self.dev.write(p, &tmp)?;
                }
            }
        }
        inode.set_size(len);
        let now = self.now();
        inode.touch(now);
        self.write_inode(ino, &mut inode)
    }
}

/// Reads inode `ino` through `cache` and checks its checksum (an all-zero inode, never used,
/// passes).
pub(crate) fn read_inode_raw<D: BlockDevice>(dev: &mut D, cache: &mut Cache, geo: &Geometry, seed: u32, ino: Ino) -> Result<Inode> {
    if ino == 0 || ino > geo.total_inodes() {
        return Err(Error::Corrupt("inode number"));
    }
    let g = (ino - 1) / geo.inodes_per_group;
    let index = (ino - 1) % geo.inodes_per_group;
    let block = geo.block_bitmap(g) + 2 + (index / INODES_PER_BLOCK) as u64;
    let at = (index % INODES_PER_BLOCK) as usize * INODE_SIZE;
    let data = cache.get(dev, block)?;
    let inode = Inode { raw: data[at..at + INODE_SIZE].try_into().unwrap() };
    if !inode.checksum_ok(seed, ino) && inode.raw.iter().any(|&b| b != 0) {
        return Err(Error::Corrupt("inode checksum"));
    }
    Ok(inode)
}
