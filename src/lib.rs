//! A small ext4 for one firmware: it formats, mounts, reads and writes the file systems it
//! makes itself, with a jbd2 journal for crash safety.
//!
//! What it makes (see [`format`]): 4 KiB blocks, 32768 blocks a group, no `flex_bg`, no
//! `64bit`, 256-byte inodes, extents, `metadata_csum`, `sparse_super`, linear directories (no
//! htree), and an internal journal (inode 8, jbd2 checksum v3). [`Ext4::mount`] refuses a file
//! system with anything else, so it never writes one it does not fully understand.
//!
//! Crash safety: metadata goes through the journal, file data does not (as Linux's
//! `data=ordered`): file data reaches the device before the transaction that makes it part of
//! a file commits. A transaction is written to the log, committed, written to its home blocks
//! and then dropped from the log ([`Ext4::sync`]), so the log holds at most one transaction and
//! needs no revoke records. [`Ext4::mount`] replays a committed one that a power loss left.
//!
//! No dependencies; `no_std` with `alloc`.

#![no_std]

extern crate alloc;

mod alloc_bits;
mod bytes;
mod cache;
mod check;
mod crc32c;
mod dir;
mod extent;
mod fs;
mod inode;
mod journal;
mod layout;
mod mkfs;

pub use check::CheckReport;
pub use fs::{DirEntry, Ext4, Ino, Kind, Metadata, MountOptions, Stats};
pub use mkfs::{FormatOptions, format};

/// The block size of every file system this crate makes or mounts.
pub const BLOCK_SIZE: usize = 4096;

/// The root directory's inode.
pub const ROOT_INO: Ino = 2;

/// The device failed a read, write or flush.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IoError;

/// A partition, in [`BLOCK_SIZE`] blocks from its start.
///
/// Buffers are whole blocks. The device may keep writes in a cache until [`flush`]; it must
/// not reorder a write before an earlier `flush`.
///
/// [`flush`]: BlockDevice::flush
pub trait BlockDevice {
    /// Reads `buf.len() / BLOCK_SIZE` blocks starting at `block`.
    fn read(&mut self, block: u64, buf: &mut [u8]) -> core::result::Result<(), IoError>;
    /// Writes `data.len() / BLOCK_SIZE` blocks starting at `block`.
    fn write(&mut self, block: u64, data: &[u8]) -> core::result::Result<(), IoError>;
    /// Returns once every earlier write is stored.
    fn flush(&mut self) -> core::result::Result<(), IoError>;
    /// The partition's size in blocks.
    fn blocks(&self) -> u64;
    /// Seconds since 1970 for timestamps (0 when the device has no clock).
    fn now(&self) -> u32 {
        0
    }
}

/// Why an operation failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// The device failed. After a failed write the file system is read-only until mounted
    /// again (the journal keeps it consistent).
    Io,
    /// A path component does not exist.
    NotFound,
    /// The name exists already.
    Exists,
    /// A directory was expected.
    NotDir,
    /// A file was expected.
    IsDir,
    /// The directory is not empty.
    NotEmpty,
    /// No free blocks or inodes.
    NoSpace,
    /// A path component longer than 255 bytes, empty, `.` or `..`, or holding a NUL.
    BadName,
    /// Past the largest file size (2^32 blocks).
    TooLarge,
    /// A failed write made the file system read-only.
    ReadOnly,
    /// The on-disk structures are inconsistent.
    Corrupt(&'static str),
    /// A file system this crate did not make, or a feature it does not handle.
    Unsupported(&'static str),
}

impl From<IoError> for Error {
    fn from(_: IoError) -> Self {
        Error::Io
    }
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Error::Io => f.write_str("device I/O failed"),
            Error::NotFound => f.write_str("not found"),
            Error::Exists => f.write_str("exists already"),
            Error::NotDir => f.write_str("not a directory"),
            Error::IsDir => f.write_str("is a directory"),
            Error::NotEmpty => f.write_str("directory not empty"),
            Error::NoSpace => f.write_str("no space left"),
            Error::BadName => f.write_str("bad name"),
            Error::TooLarge => f.write_str("file too large"),
            Error::ReadOnly => f.write_str("read-only after a failed write"),
            Error::Corrupt(what) => write!(f, "corrupt: {}", what),
            Error::Unsupported(what) => write!(f, "unsupported: {}", what),
        }
    }
}

impl core::error::Error for Error {}

pub type Result<T> = core::result::Result<T, Error>;
