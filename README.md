# ext4_rs (ferrite fork)

A small `no_std` ext4 with a jbd2 journal, for the ferrite-ui-xl firmware (Exynos 4412
tablet, eMMC USERDATA partition). No dependencies; needs `alloc`.

Forked from [yuoo655/ext4_rs](https://github.com/yuoo655/ext4_rs); the code here is a rewrite
(the fork's history keeps the original).

## What it does

- `format` makes one shape of file system: 4 KiB blocks, 32768 blocks a group, 256-byte
  inodes, extents, `metadata_csum`, `sparse_super`, `large_file`, `dir_nlink`,
  `extra_isize`, linear directories (no htree), no `flex_bg`/`64bit`, an internal journal
  (inode 8, jbd2 checksum v3). Every inode table is zeroed.
- `Ext4::mount` accepts only that shape (also when Linux's mke2fs made it with those
  features, or e2fsck left groups uninitialized), replays a committed transaction, and marks
  the file system in use (`needs_recovery`) until `unmount`.
- Files and directories: `lookup`, `metadata`, `read_dir`, `create`, `mkdir`, `remove`,
  `rename` (replaces a file atomically), `read_at`, `write_at`, `set_len`, `sync`, `stats`.
  Paths are `/`-separated (any name of 1–255 bytes without `/` or NUL).

## Crash safety

Metadata goes through the journal; file data does not (Linux's `data=ordered`): data reaches
the device before the transaction that makes it part of a file. `sync` writes the
transaction to the log, commits it, writes it home and empties the log; a transaction also
commits by itself once it holds `MountOptions::commit_blocks` blocks. After a power loss the
file system is as of the last commit (mount replays it). A failed device write makes it
read-only until mounted again.

Replace a file safely by writing a new one and `rename`-ing it over the old.

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

## Tests

`cargo test --release`. The integration tests check every image with e2fsprogs in WSL
(`wsl -d Debian`: `e2fsck -fn`, `debugfs`, `mke2fs`): files and directories, deep extent
trees, a full device, a power loss after every device write (and with writes reordered
between flushes, replayed by this crate and by `e2fsck -fy`), failed writes, and interchange
with images and files Linux's tools wrote. Set `EXT4_NO_E2FSPROGS=1` to skip the e2fsprogs
parts.

The on-disk format follows the kernel's `Documentation/filesystems/ext4`; no kernel code is
used.
