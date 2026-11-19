# ext4_rs

A small, journaled ext4 for `no_std` firmware. It formats a partition, mounts it, and reads
and writes files and directories with a jbd2 journal, so a power loss never leaves the file
system inconsistent.

- `#![no_std]`, needs only `alloc`. No dependencies, no `unsafe`.
- Builds for bare-metal targets such as `thumbv7m-none-eabi`, `armv7a-none-eabihf` and
  `riscv32imc-unknown-none-elf` (no atomics needed).
- Images it makes are ordinary ext4: Linux mounts them, and `e2fsck` finds them clean.
- Built-in consistency check (`check()`), a small `e2fsck -n` for devices where e2fsck cannot
  run.

Forked from [yuoo655/ext4_rs](https://github.com/yuoo655/ext4_rs). The code on `main` is a
rewrite; the upstream code is on `refactor`.

> **It mounts only the file systems it makes itself** (or ones that Linux's `mke2fs` made with
> the same features, see [Making an image on Linux](#making-an-image-on-linux)). It refuses any
> other ext4, such as a default Linux rootfs, with `Error::Unsupported` before writing
> anything. It is meant for a partition that your firmware owns, not for reading arbitrary
> ext4 disks.

## Installation

```toml
[dependencies]
ext4_rs = { git = "https://github.com/erhanacunal/ext4_rs", branch = "main" }
```

To pin a version, use `rev = "<commit>"` instead of `branch`.

## Quick start

### 1. Connect your storage

Implement `BlockDevice` for your partition: an SD card, eMMC, NOR/NAND behind an FTL, a USB
drive, or a RAM disk. Blocks are always 4 KiB (`BLOCK_SIZE`), and block 0 is the start of the
partition.

```rust
use ext4_rs::{BLOCK_SIZE, BlockDevice, IoError};

struct SdPartition {
    first_sector: u64, // partition start, in 512-byte sectors
    blocks: u64,       // partition size, in 4 KiB blocks
    // ... your driver
}

impl BlockDevice for SdPartition {
    fn read(&mut self, block: u64, buf: &mut [u8]) -> Result<(), IoError> {
        // buf.len() is a multiple of BLOCK_SIZE: read buf.len() / BLOCK_SIZE blocks.
        let sector = self.first_sector + block * (BLOCK_SIZE as u64 / 512);
        sd_read(sector, buf).map_err(|_| IoError)
    }

    fn write(&mut self, block: u64, data: &[u8]) -> Result<(), IoError> {
        let sector = self.first_sector + block * (BLOCK_SIZE as u64 / 512);
        sd_write(sector, data).map_err(|_| IoError)
    }

    fn flush(&mut self) -> Result<(), IoError> {
        // Return only once every earlier write is stored (e.g. send CMD6 cache flush / SYNC).
        sd_flush().map_err(|_| IoError)
    }

    fn blocks(&self) -> u64 {
        self.blocks
    }

    // Optional: seconds since 1970 for file timestamps. Leave it out if there is no clock.
    fn now(&self) -> u32 {
        rtc_seconds()
    }
}
```

The device may cache writes and reorder them, but **it must not move a write before an earlier
`flush`**. The journal relies on that ordering.

### 2. Mount, or format on first boot

`mount` takes the device by value, so on failure the device is gone; recreate the handle
(usually a cheap struct) and format only when no ext4 superblock is found:

```rust
use ext4_rs::{Error, Ext4, FormatOptions, MountOptions, format};

fn open() -> Result<Ext4<SdPartition>, Error> {
    match Ext4::mount(SdPartition::new(), MountOptions::default()) {
        Err(Error::Unsupported("no ext4 superblock")) => {
            // Empty partition: format it, then mount.
            let mut dev = SdPartition::new();
            format(&mut dev, &FormatOptions {
                uuid: random_uuid(), // two file systems should never share a UUID
                label: *b"data\0\0\0\0\0\0\0\0\0\0\0\0",
                ..FormatOptions::default()
            })?;
            Ext4::mount(dev, MountOptions::default())
        }
        other => other,
    }
}
```

Do not format on every `Err`: `Error::Corrupt` or `Error::Io` would then wipe a file system
that might still be readable, and `Unsupported` for other reasons means someone else's ext4.

If the previous session was cut off, `mount` replays the last committed transaction by
itself. `fs.mount_state()` returns `(was_unclean, replayed)` if you want to log it.

### 3. Files and directories

```rust
use ext4_rs::{Error, Kind};

fn demo(fs: &mut Ext4<SdPartition>) -> Result<(), Error> {
    fs.mkdir("/config")?;

    // Create a file and write to it. write_at returns the bytes written,
    // fewer than asked only when the device is full.
    let ino = fs.create("/config/settings.txt")?;
    fs.write_at(ino, 0, b"brightness=80\n")?;
    fs.sync()?; // durable from here on

    // Find it again by path and read it.
    if let Some(ino) = fs.lookup("/config/settings.txt")? {
        let meta = fs.metadata(ino)?;
        let mut buf = alloc::vec![0u8; meta.size as usize];
        let n = fs.read_at(ino, 0, &mut buf)?;
        assert_eq!(&buf[..n], b"brightness=80\n");
    }

    // List a directory ("." and ".." are left out). Return false to stop early.
    let dir = fs.lookup("/config")?.ok_or(Error::NotFound)?;
    fs.read_dir(dir, |entry| {
        let kind = if entry.kind == Kind::Dir { "dir " } else { "file" };
        log(kind, entry.name); // name is &[u8]
        true
    })?;

    fs.set_len(ino, 0)?;            // truncate (or extend with a hole)
    fs.remove("/config/settings.txt")?;
    fs.remove("/config")?;          // directories must be empty
    Ok(())
}
```

Paths are absolute and `/`-separated. A name is 1–255 bytes without `/` or NUL, and names are
case-sensitive. `lookup` returns `Ok(None)` when the last component is missing and
`Err(NotFound)` when a directory on the way is missing.

### 4. Saving a file safely

Overwriting a file in place can leave it half-written after a power loss. Write a new file and
`rename` it over the old one: `rename` is a single transaction, so after a crash the path holds
either the complete old file or the complete new one.

```rust
fn save(fs: &mut Ext4<SdPartition>, path: &str, data: &[u8]) -> Result<(), Error> {
    let tmp = "/config/.settings.tmp";
    if fs.lookup(tmp)?.is_some() {
        fs.remove(tmp)?; // left over from a cut-off save
    }
    let ino = fs.create(tmp)?;
    if fs.write_at(ino, 0, data)? < data.len() {
        fs.remove(tmp)?;
        return Err(Error::NoSpace);
    }
    fs.rename(tmp, path)?; // replaces `path` atomically
    fs.sync()
}
```

### 5. Unmount

```rust
let dev = fs.unmount()?; // commits, marks the file system clean, returns the device
```

Without `unmount` (a power cut, a reset) nothing is lost that `sync` already made durable; the
next `mount` replays the journal.

## A complete example on a RAM disk

This runs anywhere with `alloc`, and is a good way to try the API before wiring up real
storage:

```rust
extern crate alloc;
use alloc::vec::Vec;
use ext4_rs::{BLOCK_SIZE, BlockDevice, Ext4, FormatOptions, IoError, MountOptions, format};

struct RamDisk(Vec<u8>);

impl BlockDevice for RamDisk {
    fn read(&mut self, block: u64, buf: &mut [u8]) -> Result<(), IoError> {
        let at = block as usize * BLOCK_SIZE;
        buf.copy_from_slice(self.0.get(at..at + buf.len()).ok_or(IoError)?);
        Ok(())
    }
    fn write(&mut self, block: u64, data: &[u8]) -> Result<(), IoError> {
        let at = block as usize * BLOCK_SIZE;
        self.0.get_mut(at..at + data.len()).ok_or(IoError)?.copy_from_slice(data);
        Ok(())
    }
    fn flush(&mut self) -> Result<(), IoError> {
        Ok(())
    }
    fn blocks(&self) -> u64 {
        (self.0.len() / BLOCK_SIZE) as u64
    }
}

fn main() -> Result<(), ext4_rs::Error> {
    let mut dev = RamDisk(alloc::vec![0; 64 << 20]); // 64 MiB
    format(&mut dev, &FormatOptions { uuid: [7; 16], ..FormatOptions::default() })?;

    let mut fs = Ext4::mount(dev, MountOptions { cache_blocks: 64, commit_blocks: 64 })?;
    fs.mkdir("/logs")?;
    let ino = fs.create("/logs/boot.txt")?;
    fs.write_at(ino, 0, b"hello, ext4\n")?;

    let mut buf = [0u8; 32];
    let n = fs.read_at(ino, 0, &mut buf)?;
    assert_eq!(&buf[..n], b"hello, ext4\n");

    let report = fs.check()?;
    assert!(report.problems.is_empty());

    let dev = fs.unmount()?;
    // The image in `dev.0` can be written to a file and mounted on Linux (`mount -o loop`).
    let _ = dev;
    Ok(())
}
```

## Configuration

### `MountOptions`: memory use

| Field           | Default | Meaning                                                                 |
|-----------------|---------|-------------------------------------------------------------------------|
| `cache_blocks`  | 1024    | Clean metadata blocks kept in RAM (4 KiB each).                         |
| `commit_blocks` | 1024    | A transaction commits by itself once it holds this many blocks (min 16, capped by the journal size). Dirty blocks stay in RAM until then. |

With the defaults the crate can use about 8 MiB at its peak. On a microcontroller, lower both:
`MountOptions { cache_blocks: 32, commit_blocks: 32 }` needs about 256 KiB, at the cost of
more device reads and more frequent commits. Each mounted file system also keeps its group
descriptors in RAM (32 bytes for every 128 MiB of partition).

### `FormatOptions`

| Field              | Default | Meaning                                                              |
|--------------------|---------|----------------------------------------------------------------------|
| `uuid`             | zeros   | The volume UUID. Use a random one.                                   |
| `label`            | zeros   | The volume label, NUL-padded to 16 bytes.                            |
| `inodes_per_group` | 2048    | A multiple of 16. 2048 is one inode for every 64 KiB; raise it for many small files. |
| `journal_blocks`   | 8192    | The journal size in 4 KiB blocks (32 MiB). At least 1024, and it must fit in the first 128 MiB. |

`format` returns `Error::NoSpace` if the partition is too small for the metadata and the
journal. Partitions up to 16 TiB (2^32 blocks) are supported.

## On-disk format

`format` makes one shape of file system, and `mount` accepts only that shape:

- 4 KiB blocks, 32768 blocks per group, 256-byte inodes
- `extents`, `metadata_csum`, `sparse_super`, `large_file`, `dir_nlink`, `extra_isize`
- linear directories (no htree), no `flex_bg`, no `64bit`
- an internal journal (inode 8, jbd2 checksum v3); every inode table is zeroed

The format follows the kernel's `Documentation/filesystems/ext4`; no kernel code is used.

### Making an image on Linux

To prepare a partition on a PC instead of with `format`, create it with matching features:

```sh
mke2fs -t ext4 -b 4096 -I 256 -g 32768 -J size=32 \
  -O ^64bit,^flex_bg,^resize_inode,^dir_index,^huge_file,^metadata_csum_seed,^orphan_file,^ext_attr \
  -E lazy_itable_init=0,lazy_journal_init=0 /dev/sdX1
```

Files that Linux writes onto such a file system (with `mount` or `debugfs`) read back with this
crate, and files this crate writes read back on Linux.

## Crash safety

Metadata goes through the journal; file data does not (like Linux's `data=ordered`): data
reaches the device before the transaction that makes it part of a file commits.

- `sync()` writes the running transaction to the log, commits it, writes it to its home blocks
  and empties the log. The log never holds more than one transaction (no revoke records).
- A transaction also commits by itself once it holds `commit_blocks` blocks.
- After a power loss, the file system is as of the last commit; `mount` replays it.
- A failed device write makes the file system read-only (`Error::ReadOnly`) until it is
  mounted again. The half-done transaction never commits.

`commit_to_log_only()` is for tests on real devices: it commits the running transaction to the
log only, as a power cut right after the commit block would leave it, and makes the file
system read-only. The next mount must replay it.

### Checking a file system

```rust
let report = fs.check()?; // commits pending changes first
if !report.problems.is_empty() {
    for p in &report.problems {
        log_error(p);
    }
}
```

`check()` walks the tree from the root and the journal, claims every block once, and compares
the bitmaps, group descriptors and superblock counters with what it reached. It also reports
counts (`dirs`, `files`, `bytes`, `used_blocks`, `used_inodes`). It needs a bitmap of the whole
partition in RAM (1 bit per 4 KiB block, e.g. 450 KiB for 14 GiB).

## Errors

| Error                | When                                                              |
|----------------------|-------------------------------------------------------------------|
| `Io`                 | The device failed a read, write or flush.                         |
| `NotFound`           | A path component does not exist.                                  |
| `Exists`             | `create` / `mkdir` on a name that is taken.                       |
| `NotDir` / `IsDir`   | A directory was expected, or a file was.                          |
| `NotEmpty`           | `remove` of a directory that has entries.                         |
| `NoSpace`            | No free blocks or inodes.                                         |
| `BadName`            | An empty name, `.`, `..`, longer than 255 bytes, or with a NUL.  |
| `TooLarge`           | Past the largest file size (2^32 blocks, 16 TiB).                 |
| `ReadOnly`           | A failed write made the file system read-only until remount.      |
| `Corrupt(&str)`      | On-disk structures are inconsistent (e.g. a bad checksum).        |
| `Unsupported(&str)`  | Not a file system this crate makes, or a feature it does not handle. |

`Error` implements `core::fmt::Display` and `core::error::Error`.

## Limitations

- Regular files and directories only: no symlinks, hard links, device nodes or extended
  attributes. Files are created `0644`, directories `0755`, owned by root.
- Linear directories: lookups in a directory are linear in its size. Keep directories to a
  few thousand entries.
- `Ext4` is used through `&mut self`. To share it between tasks, wrap it in a mutex.
- One mount at a time; nothing else may write to the partition while it is mounted.

## Tests

```sh
cargo test --release
```

The integration tests check every image with e2fsprogs (`e2fsck -fn`, `debugfs`, `mke2fs`; on
Windows they run in WSL through `wsl -d Debian`) and with `check()`. They cover files and
directories, deep extent trees, a full device, a power loss after every device write (also
with writes reordered between flushes, replayed both by this crate and by `e2fsck -fy`, which
must agree), a commit cut after the commit block, failed writes, damage `check()` must find,
and interchange with images and files Linux's tools wrote. Set `EXT4_NO_E2FSPROGS=1` to skip
the e2fsprogs parts.

It has also been tested on real hardware (eMMC, 13.8 GiB partition): formatting and copying
the initial files took 7.3 s, and repeated power cuts under a write load (safe rewrites,
appends, create/delete churn, 4 MiB rewrites) all came back consistent, with every committed
file whole.

## License

MIT. See [LICENSE](LICENSE).
