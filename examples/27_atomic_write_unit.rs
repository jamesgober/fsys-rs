//! # `Handle::atomic_write_unit()` — NAWUN/NAWUPF probe
//!
//! 0.9.4 added an NVMe Identify Namespace probe that reads the
//! drive's **N**amespace **A**tomic **W**rite **U**nit **N**ormal
//! (NAWUN) / **N**amespace **A**tomic **W**rite **U**nit
//! **P**ower-**F**ail (NAWUPF) fields. The result is exposed as:
//!
//! ```ignore
//! pub fn atomic_write_unit(&self) -> Option<u32>;
//! ```
//!
//! - `Some(n)` — the drive guarantees that any write of `<= n`
//!   bytes will either land entirely or not at all, even across
//!   power failure. No torn-write detection or CRC is needed.
//! - `None` — the drive doesn't expose NAWUN, the probe failed,
//!   or fsys couldn't read the namespace identifier.
//!
//! Typical values on guaranteeing drives: 4096, 16384, 32768.
//! Consumer NVMe usually reports nothing (or 512, which is the
//! sector size — not useful for application-level torn-write
//! detection).
//!
//! ## When to use this pattern
//!
//! Database engines that ship a CRC-32C / page-checksum layer on
//! every page write. If `atomic_write_unit() >= page_size`, the
//! engine can **skip the per-page checksum** because torn writes
//! cannot happen. That's a meaningful win on hot-path page IO.
//!
//! Filesystem engines doing journal commits where the commit
//! record is `<= atomic_write_unit()` bytes — same logic.
//!
//! ## When NOT to use this pattern
//!
//! - Consumer NVMe: NAWUN is unreported or 512. Keep the per-page
//!   torn-write detection.
//! - Cross-platform: the probe runs on Linux only. macOS and
//!   Windows always return `None`.
//! - Mixed drive deployments: NAWUN is per-drive, not per-host.
//!   If a Handle's filesystem spans multiple drives, the lowest
//!   common denominator applies.
//!
//! Run: `cargo run --example 27_atomic_write_unit`

fn main() -> fsys::Result<()> {
    let fs = fsys::builder().method(fsys::Method::Direct).build()?;

    let nawun = fs.atomic_write_unit();
    println!("NVMe atomic_write_unit probe:");
    match nawun {
        Some(n) => {
            println!("  drive reports NAWUN = {n} bytes");
            // Decision: would a database engine skip per-page CRC?
            let page_size = 4096_u32;
            if n >= page_size {
                println!(
                    "  ✓ NAWUN ≥ {page_size} (typical DB page size) — engine MAY skip per-page CRC"
                );
                println!("    (still required: handle in-flight writes that span pages)");
            } else {
                println!("  ✗ NAWUN < {page_size} — per-page CRC still required");
            }
        }
        None => {
            println!("  drive does not expose NAWUN");
            println!("  ✗ application MUST keep per-page CRC / torn-write detection");
            println!();
            println!("  causes:");
            println!("    - non-NVMe storage (HDD, SATA SSD)");
            println!("    - consumer NVMe (most don't expose NAWUN)");
            println!("    - non-Linux platform (probe is Linux-only)");
            println!("    - NVMe Identify Namespace command rejected");
        }
    }

    println!();
    println!("note: NAWUN is per-drive; this Handle's filesystem may span");
    println!("multiple drives in a RAID / LVM setup. Test on the production");
    println!("storage layout before depending on the guarantee.");

    Ok(())
}
