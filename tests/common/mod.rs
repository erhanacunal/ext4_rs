//! Test devices and the e2fsprogs checks (run in WSL: `wsl -d Debian`).
#![allow(dead_code)]

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::Command;

use ext4_rs::{BLOCK_SIZE, BlockDevice, IoError};

/// A sparse in-memory disk. With `log`, every write and flush is recorded, so a test can
/// rebuild the disk as it was at any point (a power loss there).
#[derive(Clone, Default)]
pub struct MemDisk {
    pub blocks: u64,
    pub data: BTreeMap<u64, Vec<u8>>,
    pub log: Option<Vec<Event>>,
    pub now: u32,
    /// Fail every write once this many more have been made.
    pub fail_after: Option<usize>,
}

#[derive(Clone)]
pub enum Event {
    Write(u64, Vec<u8>),
    Flush,
}

impl MemDisk {
    pub fn new(mib: u64) -> Self {
        Self { blocks: mib * 256, now: 1_763_000_000, ..Default::default() }
    }

    /// The disk with only the first `n` logged events applied to `base`.
    pub fn replay_log(base: &MemDisk, log: &[Event], n: usize) -> MemDisk {
        let mut d = base.clone();
        d.log = None;
        for e in &log[..n] {
            if let Event::Write(b, data) = e {
                for (i, chunk) in data.chunks(BLOCK_SIZE).enumerate() {
                    d.data.insert(b + i as u64, chunk.to_vec());
                }
            }
        }
        d
    }

    /// Writes the image to `target/ext4-tests/NAME.img`; returns its path.
    pub fn save(&self, name: &str) -> PathBuf {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target").join("ext4-tests");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("{}.img", name));
        let mut bytes = vec![0u8; (self.blocks as usize) * BLOCK_SIZE];
        for (&b, data) in &self.data {
            bytes[b as usize * BLOCK_SIZE..(b as usize + 1) * BLOCK_SIZE].copy_from_slice(data);
        }
        std::fs::write(&path, bytes).unwrap();
        path
    }

    pub fn load(path: &PathBuf) -> MemDisk {
        let bytes = std::fs::read(path).unwrap();
        let mut d = MemDisk { blocks: (bytes.len() / BLOCK_SIZE) as u64, now: 1_763_000_000, ..Default::default() };
        for (i, chunk) in bytes.chunks(BLOCK_SIZE).enumerate() {
            if chunk.iter().any(|&b| b != 0) {
                d.data.insert(i as u64, chunk.to_vec());
            }
        }
        d
    }
}

impl BlockDevice for MemDisk {
    fn read(&mut self, block: u64, buf: &mut [u8]) -> Result<(), IoError> {
        assert!(buf.len() % BLOCK_SIZE == 0 && !buf.is_empty());
        for (i, chunk) in buf.chunks_mut(BLOCK_SIZE).enumerate() {
            let b = block + i as u64;
            assert!(b < self.blocks, "read past the end: {}", b);
            match self.data.get(&b) {
                Some(d) => chunk.copy_from_slice(d),
                None => chunk.fill(0),
            }
        }
        Ok(())
    }

    fn write(&mut self, block: u64, data: &[u8]) -> Result<(), IoError> {
        assert!(data.len() % BLOCK_SIZE == 0 && !data.is_empty());
        if let Some(n) = &mut self.fail_after {
            if *n == 0 {
                return Err(IoError);
            }
            *n -= 1;
        }
        for (i, chunk) in data.chunks(BLOCK_SIZE).enumerate() {
            let b = block + i as u64;
            assert!(b < self.blocks, "write past the end: {}", b);
            if chunk.iter().all(|&x| x == 0) {
                self.data.remove(&b);
            } else {
                self.data.insert(b, chunk.to_vec());
            }
        }
        if let Some(log) = &mut self.log {
            log.push(Event::Write(block, data.to_vec()));
        }
        Ok(())
    }

    fn flush(&mut self) -> Result<(), IoError> {
        if let Some(log) = &mut self.log {
            log.push(Event::Flush);
        }
        Ok(())
    }

    fn blocks(&self) -> u64 {
        self.blocks
    }

    fn now(&self) -> u32 {
        self.now
    }
}

/// `/mnt/e/...` for a Windows path.
fn wsl_path(p: &PathBuf) -> String {
    let s = p.to_string_lossy().replace('\\', "/");
    let (drive, rest) = s.split_once(':').unwrap();
    format!("/mnt/{}{}", drive.to_lowercase(), rest)
}

pub fn have_e2fsprogs() -> bool {
    std::env::var("EXT4_NO_E2FSPROGS").is_err()
}

/// Runs a command in WSL; returns its exit code and output.
pub fn wsl(args: &[&str]) -> (i32, String) {
    let out = Command::new("wsl.exe").args(["-d", "Debian", "--exec"]).args(args).output().expect("wsl");
    let text = String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr);
    (out.status.code().unwrap_or(-1), text)
}

/// `e2fsck -fn` must find nothing.
pub fn fsck_clean(disk: &MemDisk, name: &str) {
    if !have_e2fsprogs() {
        return;
    }
    let path = disk.save(name);
    let (code, text) = wsl(&["/usr/sbin/e2fsck", "-fn", &wsl_path(&path)]);
    assert_eq!(code, 0, "e2fsck -fn {}:\n{}", name, text);
}

/// `e2fsck -fy` (replaying the journal as Linux would), then `e2fsck -fn` must be clean.
/// Returns the repaired disk.
pub fn fsck_repair(disk: &MemDisk, name: &str) -> MemDisk {
    let path = disk.save(name);
    let (code, text) = wsl(&["/usr/sbin/e2fsck", "-fy", &wsl_path(&path)]);
    assert!(code == 0 || code == 1, "e2fsck -fy {} exited {}:\n{}", name, code, text);
    assert!(!text.contains("Fix? yes") && !text.contains("Clear? yes"), "e2fsck -fy {} fixed something:\n{}", name, text);
    let (code, text) = wsl(&["/usr/sbin/e2fsck", "-fn", &wsl_path(&path)]);
    assert_eq!(code, 0, "e2fsck -fn {} after -fy:\n{}", name, text);
    MemDisk::load(&path)
}

/// `debugfs -R "cat PATH"` on the saved image.
pub fn debugfs_cat(disk: &MemDisk, name: &str, file: &str) -> Vec<u8> {
    let path = disk.save(name);
    let out = Command::new("wsl.exe")
        .args(["-d", "Debian", "--exec", "/usr/sbin/debugfs", "-R", &format!("cat {}", file), &wsl_path(&path)])
        .output()
        .expect("wsl");
    out.stdout
}

/// Runs a shell command in WSL with `IMG` set to the image's path.
pub fn wsl_sh(path: &PathBuf, script: &str) -> (i32, String) {
    wsl(&["sh", "-c", &format!("IMG='{}'; {}", wsl_path(path), script)])
}

/// Deterministic test bytes.
pub fn pattern(seed: u64, len: usize) -> Vec<u8> {
    let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x as u8
        })
        .collect()
}
