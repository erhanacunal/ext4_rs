# ext4_rs (ferrite fork)

A small `no_std` ext4 with a jbd2 journal, for the ferrite-ui-xl firmware (Exynos 4412
tablet, eMMC USERDATA partition, ~13.8 GiB). No dependencies; needs `alloc`.

Forked from [yuoo655/ext4_rs](https://github.com/yuoo655/ext4_rs); the code on `main` is a
rewrite (the upstream code is on `refactor`).

```toml
ext4_rs = { git = "https://github.com/erhanacunal/ext4_rs", branch = "main" }
```

## What it does

- `format` makes one shape of file system: 4 KiB blocks, 32768 blocks a group, 256-byte
  inodes, extents, `metadata_csum`, `sparse_super`, `large_file`, `dir_nlink`,
  `extra_isize`, linear directories (no htree), no `flex_bg`/`64bit`, an internal journal
  (inode 8, jbd2 checksum v3). Every inode table is zeroed.
- `Ext4::mount` accepts only that shape (also when Linux's mke2fs made it with those
  features, or e2fsck left groups uninitialized), replays a committed transaction, and marks
  the file system in use (`needs_recovery`) until `unmount`. Anything else is refused with
  `Error::Unsupported` before a single write, so a foreign ext4 (a Linux rootfs) is left
  alone. `mount_state()` says whether it found the file system in use and replayed a
  transaction.
- Files and directories: `lookup`, `metadata`, `read_dir`, `create`, `mkdir`, `remove`,
  `rename` (replaces a file atomically), `read_at`, `write_at`, `set_len`, `sync`, `stats`.
  Paths are `/`-separated (any name of 1–255 bytes without `/` or NUL); names are
  case-sensitive.
- `check()`: a consistency check of the whole file system, a small `e2fsck -n` for devices
  where e2fsck cannot run. It walks the tree from the root and the journal, claims every block
  once, and compares the bitmaps, group descriptors and superblock counters with what it
  reached; problems come back as text in a `CheckReport`.

## Crash safety

Metadata goes through the journal; file data does not (Linux's `data=ordered`): data reaches
the device before the transaction that makes it part of a file. `sync` writes the
transaction to the log, commits it, writes it home and empties the log, so the log never
holds more than one transaction (no revoke records). A transaction also commits by itself
once it holds `MountOptions::commit_blocks` blocks. After a power loss the file system is as
of the last commit (mount replays it). A failed device write makes it read-only until
mounted again; the half-done transaction never commits.

Replace a file safely by writing a new one and `rename`-ing it over the old.

`commit_to_log_only()` is for tests on real devices: it commits the running transaction to
the log only, as a power cut right after the commit block would leave it, and makes the file
system read-only; the next mount must replay it.

## Device

```rust
impl BlockDevice for MyPartition {
    fn read(&mut self, block: u64, buf: &mut [u8]) -> Result<(), IoError> { … }   // whole 4 KiB blocks
    fn write(&mut self, block: u64, data: &[u8]) -> Result<(), IoError> { … }
    fn flush(&mut self) -> Result<(), IoError> { … }   // earlier writes are stored
    fn blocks(&self) -> u64 { … }
    fn now(&self) -> u32 { … }                          // seconds since 1970, or 0
}
```

A write may be reordered with other writes, but not across a `flush`.

## Tests

`cargo test --release`. The integration tests check every image with e2fsprogs in WSL
(`wsl -d Debian`: `e2fsck -fn`, `debugfs`, `mke2fs`) and with `check()`: files and
directories, deep extent trees, a full device, a power loss after every device write (and
with writes reordered between flushes, replayed by this crate and by `e2fsck -fy`, which
must agree), a commit cut after the commit block, failed writes, damage `check()` must find,
and interchange with images and files Linux's tools wrote. Set `EXT4_NO_E2FSPROGS=1` to skip
the e2fsprogs parts.

On the tablet (2026-11-17): formatting the 13.8 GiB partition and copying the firmware's
files took 7.3 s; five power cuts under a write load (safe rewrites, appends, create/delete
churn, 4 MiB rewrites) and a cut right after a commit block all came back consistent, with
every committed file whole.

The on-disk format follows the kernel's `Documentation/filesystems/ext4`; no kernel code is
used.
