//! # `Method::Spdk` runtime gating (1.1.0)
//!
//! `Method::Spdk` is the kernel-bypass backend selector. It is
//! **runtime-validated**, not compile-time reserved — the rejection
//! reason depends on the build and the host:
//!
//! - Without the `spdk` Cargo feature, `Builder::build` returns
//!   [`Error::FeatureNotEnabled { feature: "spdk" }`](fsys::Error::FeatureNotEnabled).
//! - With the feature on but the host ineligible, it returns
//!   [`Error::SpdkUnavailable { reason }`](fsys::Error::SpdkUnavailable)
//!   with a specific
//!   [`SpdkSkipReason`](fsys::capability::SpdkSkipReason).
//!
//! Most callers should rely on `Method::Auto`, which falls back
//! cleanly when SPDK is not selectable. This example is for the
//! diagnostic case — you want to know exactly *why* SPDK is or
//! isn't available on a host.
//!
//! Run: `cargo run --example 33_method_spdk_gating`

use fsys::{Builder, Error, Method};

fn main() {
    // Print the capability probe's verdict first.
    let caps = fsys::capability::capabilities();
    println!("SPDK eligibility: {}", caps.spdk_eligible);
    if !caps.spdk_skip_reasons.is_empty() {
        println!("Skip reasons:");
        for r in &caps.spdk_skip_reasons {
            println!("  • {r}");
        }
    }
    println!();

    // Try to build a handle with Method::Spdk. The outcome depends
    // on the `spdk` Cargo feature + the capability probe.
    match Builder::new().method(Method::Spdk).build() {
        Ok(_handle) => {
            println!("Method::Spdk handle constructed successfully.");
            println!("(this would only happen with `--features spdk` AND an");
            println!("eligible host AND a future `fsys-spdk` shipping the");
            println!("real backend implementation)");
        }
        Err(Error::FeatureNotEnabled { feature }) => {
            println!("FeatureNotEnabled — rebuild with `--features {feature}`");
            println!("to opt into the SPDK backend wiring. Note that with");
            println!("the feature on, the eligibility probe will still gate");
            println!("the actual handle construction.");
        }
        Err(Error::SpdkUnavailable { reason }) => {
            println!("SpdkUnavailable — fix the precondition and retry:");
            println!("  {reason}");
            println!();
            println!("See `docs/SPDK.md` for per-reason remediation steps");
            println!("(hugepages allocation, IOMMU, device binding, etc.).");
        }
        Err(other) => {
            println!("Unexpected error from Method::Spdk: {other}");
        }
    }
}
