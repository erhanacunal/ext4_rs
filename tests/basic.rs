mod common;

use common::*;
use ext4_rs::*;

fn options(uuid: u8) -> FormatOptions {
    FormatOptions {
        uuid: [uuid, 2, 3, 4, 5, 6, 0x47, 8, 0x89, 10, 11, 12, 13, 14, 15, 16],
        label: *b"ferrite-test\0\0\0\0",
        inodes_per_group: 512,
        journal_blocks: 1024,
    }
}

fn fresh(mib: u64, uuid: u8) -> MemDisk {
    let mut d = MemDisk::new(mib);
    format(&mut d, &options(uuid)).unwrap();
    d
}

fn mount(d: MemDisk) -> Ext4<MemDisk> {
    Ext4::mount(d, MountOptions::default()).unwrap()
}

#[test]
fn format_is_clean() {
    // 5 groups (0, 1 and 3 with superblock copies), the last one short.
    let d = fresh(600, 1);
    fsck_clean(&d, "format");
    let fs = mount(d);
    let stats = fs.stats();
    assert_eq!(stats.inodes, 5 * 512);
    let d = fs.unmount().unwrap();
    fsck_clean(&d, "format-mounted");
}

#[test]
fn files_roundtrip() {
    let mut fs = mount(fresh(300, 2));
    fs.mkdir("/a").unwrap();
    fs.mkdir("/a/b").unwrap();
    let sizes = [0usize, 1, 4095, 4096, 4097, 100_000, 1 << 20, (5 << 20) + 3];
    for (i, &size) in sizes.iter().enumerate() {
        let path = format!("/a/b/f{}.bin", i);
        let ino = fs.create(&path).unwrap();
        let data = pattern(i as u64, size);
        // In uneven pieces.
        let mut at = 0;
        let mut step = 777;
        while at < size {
            let n = step.min(size - at);
            assert_eq!(fs.write_at(ino, at as u64, &data[at..at + n]).unwrap(), n);
            at += n;
            step = step * 3 % 300_000 + 1;
        }
        let mut back = vec![0u8; size + 10];
        assert_eq!(fs.read_at(ino, 0, &mut back).unwrap(), size);
        assert!(back[..size] == data[..], "file {}", i);
        assert_eq!(fs.metadata(ino).unwrap().size, size as u64);
    }
    assert_eq!(fs.create("/a/b/f1.bin"), Err(Error::Exists));
    assert_eq!(fs.lookup("/a/nope").unwrap(), None);
    assert_eq!(fs.lookup("/nope/x"), Err(Error::NotFound));
    let mut names = Vec::new();
    let b = fs.lookup("/a/b").unwrap().unwrap();
    fs.read_dir(b, |e| {
        names.push(String::from_utf8(e.name.to_vec()).unwrap());
        true
    })
    .unwrap();
    assert_eq!(names.len(), sizes.len());
    let d = fs.unmount().unwrap();
    fsck_clean(&d, "roundtrip");
    if have_e2fsprogs() {
        for (i, &size) in sizes.iter().enumerate() {
            let got = debugfs_cat(&d, "roundtrip", &format!("/a/b/f{}.bin", i));
            assert!(got == pattern(i as u64, size), "debugfs file {}", i);
        }
    }
    // And again after a remount.
    let mut fs = mount(d);
    let ino = fs.lookup("/a/b/f7.bin").unwrap().unwrap();
    let mut back = vec![0u8; (5 << 20) + 3];
    fs.read_at(ino, 0, &mut back).unwrap();
    assert!(back == pattern(7, (5 << 20) + 3));
}

#[test]
fn directories_grow_and_shrink() {
    let mut fs = mount(fresh(200, 3));
    fs.mkdir("/d").unwrap();
    for i in 0..600 {
        let ino = fs.create(&format!("/d/a-rather-long-file-name-number-{:04}.txt", i)).unwrap();
        fs.write_at(ino, 0, format!("file {}", i).as_bytes()).unwrap();
    }
    for i in (0..600).step_by(2) {
        fs.remove(&format!("/d/a-rather-long-file-name-number-{:04}.txt", i)).unwrap();
    }
    for i in (1..600).step_by(4) {
        fs.rename(&format!("/d/a-rather-long-file-name-number-{:04}.txt", i), &format!("/d/r{}", i)).unwrap();
    }
    fs.mkdir("/e").unwrap();
    fs.rename("/d/r1", "/e/moved").unwrap();
    fs.mkdir("/d/sub").unwrap();
    fs.rename("/d/sub", "/e/sub").unwrap();
    assert_eq!(fs.rename("/e", "/e/sub/x"), Err(Error::BadName));
    // Replace an existing file.
    fs.rename("/d/r5", "/e/moved").unwrap();
    let ino = fs.lookup("/e/moved").unwrap().unwrap();
    let mut buf = [0u8; 16];
    let n = fs.read_at(ino, 0, &mut buf).unwrap();
    assert_eq!(&buf[..n], b"file 5");
    assert_eq!(fs.remove("/e"), Err(Error::NotEmpty));
    let mut count = 0;
    let d = fs.lookup("/d").unwrap().unwrap();
    fs.read_dir(d, |_| {
        count += 1;
        true
    })
    .unwrap();
    assert_eq!(count, 300 - 2);
    fs.sync().unwrap();
    let d = fs.unmount().unwrap();
    fsck_clean(&d, "dirs");
}

#[test]
fn fragmented_files_and_truncate() {
    let mut fs = mount(fresh(300, 4));
    let a = fs.create("/a").unwrap();
    let b = fs.create("/b").unwrap();
    // Alternating single blocks: every block of each file is its own extent, so the trees
    // grow two levels deep (4 in the inode, 340 a block).
    let n = 1500u32;
    for i in 0..n {
        fs.write_at(a, i as u64 * 4096, &pattern(i as u64, 4096)).unwrap();
        fs.write_at(b, i as u64 * 4096, &pattern(i as u64 + 100_000, 4096)).unwrap();
    }
    let check = |fs: &mut Ext4<MemDisk>, ino, blocks: u32, base: u64| {
        let mut buf = vec![0u8; 4096];
        for i in 0..blocks {
            fs.read_at(ino, i as u64 * 4096, &mut buf).unwrap();
            assert!(buf == pattern(i as u64 + base, 4096), "block {}", i);
        }
    };
    check(&mut fs, a, n, 0);
    fs.sync().unwrap();
    fsck_clean(&fs.device().clone(), "frag-full");
    fs.set_len(a, 700 * 4096 + 123).unwrap();
    check(&mut fs, a, 700, 0);
    let mut buf = vec![0u8; 4096];
    assert_eq!(fs.read_at(a, 700 * 4096, &mut buf).unwrap(), 123);
    assert!(buf[..123] == pattern(700, 4096)[..123]);
    // Growing leaves zeros, also in the rest of the cut block.
    fs.set_len(a, 800 * 4096).unwrap();
    assert_eq!(fs.read_at(a, 700 * 4096, &mut buf).unwrap(), 4096);
    assert!(buf[..123] == pattern(700, 4096)[..123] && buf[123..].iter().all(|&x| x == 0));
    assert_eq!(fs.read_at(a, 750 * 4096, &mut buf).unwrap(), 4096);
    assert!(buf.iter().all(|&x| x == 0));
    fs.write_at(a, 790 * 4096 + 5, b"hole then data").unwrap();
    check(&mut fs, b, n, 100_000);
    fs.set_len(b, 0).unwrap();
    fs.remove("/a").unwrap();
    let d = fs.unmount().unwrap();
    fsck_clean(&d, "frag-truncated");
}

#[test]
fn full_device() {
    let mut fs = mount(fresh(140, 5));
    let ino = fs.create("/big").unwrap();
    let chunk = pattern(9, 1 << 20);
    let mut total = 0u64;
    loop {
        let n = fs.write_at(ino, total, &chunk);
        match n {
            Ok(0) | Err(Error::NoSpace) => break,
            Ok(n) => total += n as u64,
            Err(e) => panic!("{:?}", e),
        }
    }
    assert!(total > 100 << 20);
    // Small files still fit in the metadata reserve? No: data cannot use it.
    let small = fs.create("/small").unwrap();
    assert_eq!(fs.write_at(small, 0, b"x"), Err(Error::NoSpace));
    fs.sync().unwrap();
    fsck_clean(&fs.device().clone(), "full");
    fs.remove("/big").unwrap();
    assert_eq!(fs.write_at(small, 0, b"x").unwrap(), 1);
    let d = fs.unmount().unwrap();
    fsck_clean(&d, "full-freed");
}

#[test]
fn rejects_bad_names() {
    let mut fs = mount(fresh(140, 6));
    for bad in ["/", "/.", "/..", "/a/..", "/x\0y"] {
        assert!(fs.create(bad).is_err(), "{:?}", bad);
    }
    let long = "/".to_string() + &"n".repeat(256);
    assert_eq!(fs.create(&long), Err(Error::BadName));
    let ok = "/".to_string() + &"n".repeat(255);
    fs.create(&ok).unwrap();
    let d = fs.unmount().unwrap();
    fsck_clean(&d, "names");
}
