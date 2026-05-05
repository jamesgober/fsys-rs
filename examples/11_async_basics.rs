//! # Async layer — `write_async` + substrate observability
//!
//! Every sync `Handle` method has an `_async` sibling. The async
//! layer is gated behind the `async` Cargo feature and requires a
//! running tokio runtime.
//!
//! On Linux + `Method::Direct`, async ops submit directly to the
//! per-handle io_uring ring (the **native substrate**, new in
//! 0.7.0). Everywhere else, async ops route through
//! `tokio::task::spawn_blocking`. Read which one a handle uses via
//! `Handle::async_substrate()`.
//!
//! Build: `cargo run --example 11_async_basics --features async`

use fsys::{builder, AsyncSubstrate};
use std::sync::Arc;

#[tokio::main]
async fn main() -> fsys::Result<()> {
    // Wrap the handle in `Arc` for cheap cloning into async tasks.
    let fs = Arc::new(builder().build()?);

    let path = std::env::temp_dir().join("fsys_example_async.txt");
    fs.clone()
        .write_async(&path, b"async write".to_vec())
        .await?;

    // Substrate observability — read which substrate this handle is
    // using *right now*. The value is computed on each call (no
    // probing), so it transitions cleanly when the io_uring ring is
    // lazily constructed on the first Direct op.
    match fs.async_substrate() {
        AsyncSubstrate::NativeIoUring => {
            println!("running on native io_uring fast path (Linux + Direct)")
        }
        AsyncSubstrate::SpawnBlocking => {
            println!("running on tokio::task::spawn_blocking fallback")
        }
        // `AsyncSubstrate` is `#[non_exhaustive]` — future variants
        // (e.g. Windows IOCP) would show up here.
        _ => println!("running on a future substrate"),
    }

    let bytes = fs.clone().read_async(&path).await?;
    println!("read back: {:?}", String::from_utf8_lossy(&bytes));

    let _ = std::fs::remove_file(&path);
    Ok(())
}
