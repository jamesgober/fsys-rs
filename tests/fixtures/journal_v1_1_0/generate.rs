//! Generator for the fsys 1.1.0 journal fixtures in this directory.
//!
//! Not compiled with the crate. The `.wal` files were produced by
//! running this program against fsys 1.1.0 (commit 03bc1bb) on Linux
//! ext4 with Direct IO active, in a scratch crate outside the
//! repository:
//!
//! ```toml
//! [dependencies]
//! fsys = { path = "<checkout of 03bc1bb>" }
//! ```
//!
//! `cargo run --release -- <output dir>`
//!
//! `tests/journal_compat_v1_1_0.rs` rebuilds the expected payloads
//! with the same functions and checks that the current reader and
//! writer handle every file.

use fsys::{JournalOptions, JournalReader};
use std::path::{Path, PathBuf};

/// 4 KiB slot, 13-byte frames: 4096 = 13 * 315 + 1, so every slot
/// rotation leaves a 1-byte zero gap.
fn gap1_payload(i: u32) -> Vec<u8> {
    vec![(i % 251) as u8]
}
const GAP1_COUNT: u32 = 700;

/// 4 KiB slot: one 13-byte frame then 15-byte frames. 13 + 15 * 272
/// = 4093, so the first rotation leaves a 3-byte zero gap (the
/// shape the 1.1.0 reader's four-zero-byte rule could not skip).
fn gap3_payload(i: u32) -> Vec<u8> {
    if i == 0 {
        vec![0xA0]
    } else {
        let b = (i % 200) as u8;
        vec![b, b.wrapping_add(1), b.wrapping_add(2)]
    }
}
const GAP3_COUNT: u32 = 600;

/// 16 KiB slot: a 100-byte record, then a 20 KiB record that does not
/// fit (rotation leaves a ~16 KiB gap, more than the 1.1.0 reader's
/// 8 KiB limit), then a small record.
fn biggap_payloads() -> Vec<Vec<u8>> {
    vec![
        vec![0x11; 100],
        (0..20 * 1024).map(|k| (k % 253) as u8).collect(),
        b"after-big-gap".to_vec(),
    ]
}

/// Two sessions: the 1.1.0 reopen resumed at the sector boundary
/// after the first session's padding, leaving a gap.
fn reopen_payloads() -> Vec<Vec<u8>> {
    let mut v: Vec<Vec<u8>> = (0..20).map(|i| format!("session-1-{i:03}").into_bytes()).collect();
    v.extend((0..20).map(|i| format!("session-2-{i:03}").into_bytes()));
    v
}

/// Buffered journal: single appends and one batch.
fn buffered_payloads() -> Vec<Vec<u8>> {
    let mut v: Vec<Vec<u8>> = (0..10).map(|i| format!("single-{i}").into_bytes()).collect();
    v.extend((0..10).map(|i| format!("batch-{i}").into_bytes()));
    v
}

fn fresh(dir: &Path, name: &str) -> PathBuf {
    let p = dir.join(name);
    let _ = std::fs::remove_file(&p);
    p
}

fn report(path: &Path) {
    let mut r = JournalReader::open(path).unwrap();
    let n = r.iter().filter_map(|x| x.ok()).count();
    println!(
        "{}: {} bytes, 1.1.0 reader -> {n} records, {:?} at {}",
        path.display(),
        std::fs::metadata(path).unwrap().len(),
        r.tail_state(),
        r.position()
    );
}

fn main() {
    let dir = PathBuf::from(std::env::args().nth(1).expect("output dir"));
    std::fs::create_dir_all(&dir).unwrap();
    let fs = fsys::builder().build().unwrap();
    let direct4k = || JournalOptions::new().direct(true).log_buffer_kib(4);

    let p = fresh(&dir, "direct_gap1.wal");
    let j = fs.journal_with(&p, direct4k()).unwrap();
    assert!(j.is_direct_active(), "Direct IO must be active");
    for i in 0..GAP1_COUNT {
        let _ = j.append(&gap1_payload(i)).unwrap();
    }
    j.close().unwrap();
    report(&p);

    let p = fresh(&dir, "direct_gap3.wal");
    let j = fs.journal_with(&p, direct4k()).unwrap();
    for i in 0..GAP3_COUNT {
        let _ = j.append(&gap3_payload(i)).unwrap();
    }
    j.close().unwrap();
    report(&p);

    let p = fresh(&dir, "direct_biggap.wal");
    let j = fs
        .journal_with(&p, JournalOptions::new().direct(true).log_buffer_kib(16))
        .unwrap();
    for r in biggap_payloads() {
        let _ = j.append(&r).unwrap();
    }
    j.close().unwrap();
    report(&p);

    let p = fresh(&dir, "direct_reopen.wal");
    let all = reopen_payloads();
    {
        let j = fs.journal_with(&p, direct4k()).unwrap();
        for r in &all[..20] {
            let _ = j.append(r).unwrap();
        }
        j.close().unwrap();
    }
    {
        let j = fs.journal_with(&p, direct4k()).unwrap();
        for r in &all[20..] {
            let _ = j.append(r).unwrap();
        }
        j.close().unwrap();
    }
    report(&p);

    let p = fresh(&dir, "buffered.wal");
    let all = buffered_payloads();
    let j = fs.journal(&p).unwrap();
    for r in &all[..10] {
        let _ = j.append(r).unwrap();
    }
    let refs: Vec<&[u8]> = all[10..].iter().map(|v| v.as_slice()).collect();
    let _ = j.append_batch(&refs).unwrap();
    j.close().unwrap();
    report(&p);
}
