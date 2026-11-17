//! Interchange with Linux's tools: a file system mke2fs and debugfs made (with this crate's
//! features) reads here; files debugfs adds to one this crate made read back here.

mod common;

use common::*;
use ext4_rs::*;

#[test]
fn reads_what_linux_wrote() {
    if !have_e2fsprogs() {
        return;
    }
    let path = MemDisk::new(300).save("linux-made");
    let (code, text) = wsl_sh(
        &path,
        "set -e; \
         /usr/sbin/mke2fs -q -F -t ext4 -b 4096 -I 256 -g 32768 -N 1536 -J size=4 \
           -O ^64bit,^flex_bg,^resize_inode,^dir_index,^huge_file,^metadata_csum_seed,^orphan_file,^ext_attr \
           -E lazy_itable_init=0,lazy_journal_init=0 -U 01020304-0506-4708-890a-0b0c0d0e0f10 \"$IMG\"; \
         head -c 3000000 /dev/urandom > /tmp/ext4rs-a; printf 'hello from linux' > /tmp/ext4rs-b; \
         /usr/sbin/debugfs -w -R 'mkdir /dir' \"$IMG\"; \
         /usr/sbin/debugfs -w -R 'write /tmp/ext4rs-a /dir/a.bin' \"$IMG\"; \
         /usr/sbin/debugfs -w -R 'write /tmp/ext4rs-b /b.txt' \"$IMG\"; \
         cp /tmp/ext4rs-a \"$IMG.a\"",
    );
    assert_eq!(code, 0, "{}", text);
    let disk = MemDisk::load(&path);
    let expect_a = std::fs::read(path.with_extension("img.a")).unwrap();
    let mut fs = Ext4::mount(disk, MountOptions::default()).unwrap();
    let a = fs.lookup("/dir/a.bin").unwrap().unwrap();
    let mut buf = vec![0u8; expect_a.len()];
    assert_eq!(fs.read_at(a, 0, &mut buf).unwrap(), expect_a.len());
    assert!(buf == expect_a);
    let b = fs.lookup("/b.txt").unwrap().unwrap();
    let mut buf = [0u8; 64];
    let n = fs.read_at(b, 0, &mut buf).unwrap();
    assert_eq!(&buf[..n], b"hello from linux");
    // And writes to it stay clean.
    let c = fs.create("/dir/c").unwrap();
    fs.write_at(c, 0, &pattern(5, 200_000)).unwrap();
    fs.remove("/b.txt").unwrap();
    let d = fs.unmount().unwrap();
    fsck_clean(&d, "linux-made-changed");
}

#[test]
fn linux_writes_into_ours() {
    if !have_e2fsprogs() {
        return;
    }
    let mut d = MemDisk::new(200);
    format(&mut d, &FormatOptions { uuid: [9; 16], inodes_per_group: 512, journal_blocks: 1024, ..Default::default() }).unwrap();
    let mut fs = Ext4::mount(d, MountOptions::default()).unwrap();
    fs.mkdir("/ours").unwrap();
    let f = fs.create("/ours/f").unwrap();
    fs.write_at(f, 0, &pattern(1, 50_000)).unwrap();
    let d = fs.unmount().unwrap();
    let path = d.save("ours-then-linux");
    let (code, text) = wsl_sh(
        &path,
        "set -e; head -c 1234567 /dev/urandom > /tmp/ext4rs-c; \
         /usr/sbin/debugfs -w -R 'write /tmp/ext4rs-c /ours/linux.bin' \"$IMG\"; \
         /usr/sbin/debugfs -w -R 'mkdir /ours/linuxdir' \"$IMG\"; \
         /usr/sbin/debugfs -w -R 'rm /ours/f' \"$IMG\"; \
         cp /tmp/ext4rs-c \"$IMG.c\"",
    );
    assert_eq!(code, 0, "{}", text);
    let expect = std::fs::read(path.with_extension("img.c")).unwrap();
    let mut fs = Ext4::mount(MemDisk::load(&path), MountOptions::default()).unwrap();
    assert_eq!(fs.lookup("/ours/f").unwrap(), None);
    let ino = fs.lookup("/ours/linux.bin").unwrap().unwrap();
    let mut buf = vec![0u8; expect.len()];
    fs.read_at(ino, 0, &mut buf).unwrap();
    assert!(buf == expect);
    fs.mkdir("/ours/linuxdir/mine").unwrap();
    let d = fs.unmount().unwrap();
    fsck_clean(&d, "ours-then-linux-again");
}
