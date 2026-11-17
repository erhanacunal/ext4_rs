//! Power loss at every point of a run of transactions: the disk as it was after the first
//! `n` writes must mount (replaying the journal), hold exactly the files of the last or the
//! next synced state, and pass `e2fsck`; `e2fsck -fy` (Linux's replay) must agree.

mod common;

use std::collections::BTreeMap;

use common::*;
use ext4_rs::*;

type State = BTreeMap<String, Vec<u8>>;

fn snapshot(fs: &mut Ext4<MemDisk>) -> State {
    fn walk(fs: &mut Ext4<MemDisk>, dir: Ino, path: &str, out: &mut State) {
        let mut entries = Vec::new();
        fs.read_dir(dir, |e| {
            entries.push((String::from_utf8(e.name.to_vec()).unwrap(), e.ino, e.kind));
            true
        })
        .unwrap();
        for (name, ino, kind) in entries {
            let p = format!("{}/{}", path, name);
            match kind {
                Kind::Dir => {
                    out.insert(p.clone() + "/", Vec::new());
                    walk(fs, ino, &p, out);
                }
                _ => {
                    let size = fs.metadata(ino).unwrap().size as usize;
                    let mut data = vec![0u8; size];
                    fs.read_at(ino, 0, &mut data).unwrap();
                    out.insert(p, data);
                }
            }
        }
    }
    let mut out = State::new();
    walk(fs, ROOT_INO, "", &mut out);
    out
}

/// The workload: each step is one transaction (synced at its end).
fn step(fs: &mut Ext4<MemDisk>, i: usize) {
    match i {
        0 => {
            fs.mkdir("/data").unwrap();
            for k in 0..20 {
                let ino = fs.create(&format!("/data/f{}", k)).unwrap();
                fs.write_at(ino, 0, &pattern(k, 1000 + k as usize * 3000)).unwrap();
            }
        }
        1 => {
            // The safe way to replace a file: write a new one, rename it over the old.
            let ino = fs.create("/data/f3.new").unwrap();
            fs.write_at(ino, 0, &pattern(333, 70_000)).unwrap();
            fs.rename("/data/f3.new", "/data/f3").unwrap();
            for k in (0..20).step_by(3) {
                if k != 3 {
                    fs.remove(&format!("/data/f{}", k)).unwrap();
                }
            }
            fs.mkdir("/data/sub").unwrap();
            fs.rename("/data/f1", "/data/sub/one").unwrap();
        }
        2 => {
            let ino = fs.create("/big").unwrap();
            fs.write_at(ino, 0, &pattern(77, 3 << 20)).unwrap();
            fs.remove("/data/f2").unwrap();
            for k in 0..100 {
                fs.create(&format!("/data/sub/empty-{}", k)).unwrap();
            }
        }
        _ => unreachable!(),
    }
    fs.sync().unwrap();
}

const STEPS: usize = 3;

#[test]
fn power_loss_anywhere() {
    let mut base = MemDisk::new(150);
    format(&mut base, &FormatOptions { uuid: [7; 16], inodes_per_group: 512, journal_blocks: 1024, ..Default::default() }).unwrap();

    // The states after each step, and the write log of the run.
    let mut states = Vec::new();
    let mut disk = base.clone();
    disk.log = Some(Vec::new());
    let mut fs = Ext4::mount(disk, MountOptions::default()).unwrap();
    states.push(snapshot(&mut fs));
    // Where each step's sync ended in the log.
    let mut ends = Vec::new();
    for i in 0..STEPS {
        step(&mut fs, i);
        states.push(snapshot(&mut fs));
        ends.push(fs.device().log.as_ref().unwrap().len());
    }
    let log = fs.device().log.clone().unwrap();
    println!("{} events, steps end at {:?}", log.len(), ends);

    // Every event boundary is a cut point; e2fsck's own replay runs on a sample.
    let mut checked_by_e2fsck = 0;
    for n in 0..=log.len() {
        let crashed = MemDisk::replay_log(&base, &log, n);
        // The step this cut falls in: state `k` or `k + 1` is acceptable.
        let k = ends.iter().position(|&e| n < e).unwrap_or(STEPS);
        let mut fs = Ext4::mount(crashed.clone(), MountOptions::default()).unwrap_or_else(|e| panic!("cut {}: {:?}", n, e));
        let now = snapshot(&mut fs);
        let ok = now == states[k] || (k < STEPS && now == states[k + 1]);
        assert!(ok, "cut {}: state matches neither step {} nor {}", n, k, k + 1);
        let after = fs.unmount().unwrap();
        if have_e2fsprogs() && (n % 7 == 0 || ends.contains(&n) || n == log.len()) {
            fsck_clean(&after, "crash-ours");
            let repaired = fsck_repair(&crashed, "crash-linux");
            let mut fs = Ext4::mount(repaired, MountOptions::default()).unwrap();
            let linux = snapshot(&mut fs);
            assert!(linux == now, "cut {}: e2fsck's replay differs from ours", n);
            checked_by_e2fsck += 1;
        }
    }
    println!("{} cuts checked with e2fsck", checked_by_e2fsck);

    // Writes between two flushes may land in any order: everything up to a flush, then only
    // one of the writes after it.
    let mut reordered = 0;
    let mut last_flush = 0;
    for (i, e) in log.iter().enumerate() {
        match e {
            Event::Flush => last_flush = i + 1,
            Event::Write(..) if i > last_flush => {
                let mut events: Vec<Event> = log[..last_flush].to_vec();
                events.push(e.clone());
                let crashed = MemDisk::replay_log(&base, &events, events.len());
                let k = ends.iter().position(|&end| last_flush < end).unwrap_or(STEPS);
                let mut fs = Ext4::mount(crashed, MountOptions::default()).unwrap_or_else(|e| panic!("write {} alone: {:?}", i, e));
                let now = snapshot(&mut fs);
                assert!(now == states[k] || (k < STEPS && now == states[k + 1]), "write {} alone", i);
                let after = fs.unmount().unwrap();
                if have_e2fsprogs() && reordered % 5 == 0 {
                    fsck_clean(&after, "crash-reordered");
                }
                reordered += 1;
            }
            _ => {}
        }
    }
    println!("{} reordered cuts", reordered);
}

#[test]
fn failed_write_makes_it_read_only() {
    let mut base = MemDisk::new(150);
    format(&mut base, &FormatOptions { uuid: [8; 16], inodes_per_group: 512, journal_blocks: 1024, ..Default::default() }).unwrap();
    let mut fs = Ext4::mount(base, MountOptions::default()).unwrap();
    fs.mkdir("/kept").unwrap();
    fs.sync().unwrap();
    let before = snapshot(&mut fs);
    let synced = fs.device().clone();
    // The device fails after `n` more writes, at each point of the commit: before the commit
    // block is written the change is gone, after it the replay finishes it.
    let mut outcomes = Vec::new();
    for n in 0..8 {
        let mut fs = Ext4::mount(synced.clone(), MountOptions::default()).unwrap();
        fs.mkdir("/new").unwrap();
        fs.device().fail_after = Some(n);
        let result = fs.sync();
        if result.is_err() {
            assert_eq!(result, Err(Error::Io));
            assert_eq!(fs.create("/x"), Err(Error::ReadOnly));
        }
        let mut disk = fs.device().clone();
        disk.fail_after = None;
        let mut fs = Ext4::mount(disk, MountOptions::default()).unwrap();
        let now = snapshot(&mut fs);
        let has_new = now.contains_key("/new/");
        let mut without = now.clone();
        without.remove("/new/");
        assert_eq!(without, before, "fail after {}", n);
        outcomes.push((n, result.is_ok(), has_new));
        let d = fs.unmount().unwrap();
        fsck_clean(&d, "failed-write");
    }
    println!("{:?}", outcomes);
    // Failing at the first write (the log) loses the change; no failure keeps it.
    assert_eq!(outcomes[0], (0, false, false));
    assert!(outcomes.last().unwrap().1 && outcomes.last().unwrap().2);
}
