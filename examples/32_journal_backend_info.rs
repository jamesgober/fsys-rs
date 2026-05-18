//! # Journal backend observability (1.1.0)
//!
//! Every [`JournalHandle`](fsys::JournalHandle) exposes three
//! accessors that let operators verify **which backend is live**:
//!
//! - [`backend_kind`](fsys::JournalHandle::backend_kind) — terse
//!   classification (kernel-buffered / kernel-direct /
//!   kernel-io-uring / spdk).
//! - [`backend_health`](fsys::JournalHandle::backend_health) —
//!   running counters (queue depth, IOPS, p99 latency).
//! - [`backend_info`](fsys::JournalHandle::backend_info) — full
//!   selection trail (chosen backend, reason, skipped fallbacks).
//!
//! Without these accessors a silent fallback (e.g. SPDK requested
//! but kernel path actually serving) would invalidate downstream
//! performance expectations. This example prints all three for the
//! default journal and for a journal opened in Direct-IO mode.
//!
//! Run: `cargo run --example 32_journal_backend_info`

use fsys::{builder, JournalOptions};

fn report(label: &str, log: &fsys::JournalHandle) {
    println!("── {label} ─────────────────────────────────");
    println!("  backend_kind:   {}", log.backend_kind());

    let health = log.backend_health();
    println!("  health.backend: {}", health.backend);
    println!(
        "    queue_depth:   {} / {} (current / max)",
        health.queue_depth_current, health.queue_depth_max
    );
    println!("    appends/sec:   {}", health.appends_per_second);
    println!(
        "    p99 latency:   {} µs (avg {} µs)",
        health.p99_append_latency_us, health.avg_append_latency_us
    );
    println!("    failed:        {}", health.failed_appends);

    let info = log.backend_info();
    println!("  info.selected:  {}", info.selected);
    println!("  info.reason:    {}", info.selection_reason);
    if !info.fallbacks_skipped.is_empty() {
        println!("  fallbacks skipped:");
        for (kind, why) in &info.fallbacks_skipped {
            println!("    • {kind}: {why}");
        }
    }
    println!();
}

fn main() -> fsys::Result<()> {
    let fs = builder().build()?;
    let dir = std::env::temp_dir();

    let buffered_path = dir.join("fsys_example_backend_info_buffered.wal");
    let _ = std::fs::remove_file(&buffered_path);
    let buffered = fs.journal(&buffered_path)?;
    report("buffered journal (default mode)", &buffered);

    let direct_path = dir.join("fsys_example_backend_info_direct.wal");
    let _ = std::fs::remove_file(&direct_path);
    // Direct mode may be rejected by the filesystem (tmpfs, some
    // FUSE mounts). On rejection, fsys silently falls back to
    // buffered mode and `backend_kind()` will report that.
    match fs.journal_with(&direct_path, JournalOptions::new().direct(true)) {
        Ok(direct) => report("direct-IO journal", &direct),
        Err(e) => println!("direct-IO journal not available on this filesystem: {e}"),
    }

    // Cleanup.
    let _ = std::fs::remove_file(&buffered_path);
    let _ = std::fs::remove_file(&direct_path);
    Ok(())
}
