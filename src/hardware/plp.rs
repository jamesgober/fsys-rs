//! Power-Loss Protection (PLP) detection refinement (0.7.0).
//!
//! Per refinement R-2 in `.dev/DECISIONS-0.7.0.md`, PLP detection uses
//! one signal: a **per-vendor model lookup table** of known
//! PLP-equipped enterprise drives (Intel D3-S4510/4610, Samsung
//! PM983/PM9A3/PM1735, Micron 7300/7400/7450, WD/HGST Ultrastar DC
//! SN-series, Kioxia CD-series). On a hit the probe reports
//! [`PlpStatus::Yes`]; otherwise `Unknown`. The probe never reports
//! `No`.
//!
//! The lookup runs on Linux (sysfs `vendor` / `model`) and Windows
//! (`IOCTL_STORAGE_QUERY_PROPERTY` vendor / product strings). The macOS
//! and fallback probes do not read device identity and always report
//! `Unknown`.
//!
//! ## What's NOT done here
//!
//! - A Volatile Write Cache (VWC) bit fallback via NVMe Identify
//!   Controller was planned in R-2 but is not implemented.
//! - Vendor-specific SMART log-page parsing. Long maintenance
//!   tail; remains out of scope.
//! - Runtime re-probing. PLP is sampled once per process and
//!   cached; hot-plug isn't supported (F-4 from 0.5.0).
//! - Per-path probing: the probe describes the drive holding the
//!   process's current directory (see [`crate::hardware::drive`]).
//!
//! ## Honesty
//!
//! Detection remains best-effort. A drive missing from the table
//! stays `Unknown`. False negatives cost
//! performance (Method::Auto picks `Direct + fdatasync` instead
//! of `Direct + NVMe FLUSH`); they are not correctness failures.
//! `docs/PLATFORM-NOTES.md` and `docs/API.md` document this.

use crate::hardware::PlpStatus;

/// Known PLP-equipped enterprise drives. Each entry is
/// `(vendor_substring, model_substring)` matched
/// case-insensitively against the drive's vendor + model strings.
/// Substring (not full-regex) matching keeps the table cheap and
/// easy to extend.
///
/// ## Maintenance
///
/// New entries land via PR with a citation: the vendor's spec
/// sheet must explicitly state PLP / "power loss protection" /
/// "supercapacitor-backed" / "tantalum-capacitor cache". Entries
/// without a citation are not accepted; "we tested it once on a
/// trade-show floor" is not a citation.
///
/// The list is intentionally conservative — false-positive PLP
/// claims would lead `Method::Auto` to skip the
/// `fdatasync` cycle on an unprotected drive, which IS a
/// correctness regression. False-negative is the safe direction
/// of travel.
// Only the Linux and Windows probes read vendor / model strings.
#[cfg_attr(not(any(target_os = "linux", target_os = "windows")), allow(dead_code))]
const PLP_DRIVE_TABLE: &[(&str, &str)] = &[
    // Intel / Solidigm enterprise SATA SSDs (D3 series).
    ("INTEL", "SSDSC2KB"),    // D3-S4510 / S4520 family
    ("INTEL", "SSDSC2KG"),    // D3-S4610 family
    ("SOLIDIGM", "SBFPF2BU"), // P5520 / P5620 enterprise NVMe
    // Samsung enterprise NVMe.
    ("SAMSUNG", "PM983"),
    ("SAMSUNG", "PM9A3"),
    ("SAMSUNG", "PM1735"),
    ("SAMSUNG", "PM1733"),
    ("SAMSUNG", "PM1725"),
    // Micron enterprise NVMe.
    ("MICRON", "7300"),
    ("MICRON", "7400"),
    ("MICRON", "7450"),
    ("MICRON", "9300"),
    ("MICRON", "9400"),
    // WD / HGST Ultrastar enterprise NVMe (DC SN series).
    ("WDC", "ULTRASTAR DC SN"),
    ("HGST", "ULTRASTAR SN"),
    ("WESTERN DIGITAL", "ULTRASTAR DC SN"),
    // Kioxia (formerly Toshiba Memory) enterprise NVMe.
    ("KIOXIA", "CD"),  // CD6/CD7/CD8 series
    ("KIOXIA", "CM"),  // CM6/CM7 series
    ("TOSHIBA", "PX"), // PX02/PX04/PX05 SAS/NVMe with PLP
    // Seagate Nytro enterprise NVMe.
    ("SEAGATE", "NYTRO"),
];

/// Returns `PlpStatus::Yes` if `(vendor, model)` matches any
/// entry in the lookup table. Otherwise `PlpStatus::Unknown`.
///
/// Matching is case-insensitive substring.
#[must_use]
// Only the Linux and Windows probes read vendor / model strings.
#[cfg_attr(not(any(target_os = "linux", target_os = "windows")), allow(dead_code))]
pub(crate) fn lookup_table(vendor: &str, model: &str) -> PlpStatus {
    let v_upper = vendor.to_ascii_uppercase();
    let m_upper = model.to_ascii_uppercase();
    for (table_vendor, table_model) in PLP_DRIVE_TABLE {
        if v_upper.contains(table_vendor) && m_upper.contains(table_model) {
            return PlpStatus::Yes;
        }
    }
    PlpStatus::Unknown
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lookup_table_matches_known_drives() {
        // Sample entries — verify the actual substrings we've
        // committed to the table.
        assert_eq!(
            lookup_table("INTEL", "SSDSC2KB019T801"),
            PlpStatus::Yes,
            "Intel D3-S4510 should be recognised"
        );
        assert_eq!(
            // Samsung enterprise drives self-report marketing
            // names via NVMe Identify (sysfs `model` reflects
            // this) — NOT the raw SKU code on the part label.
            lookup_table("Samsung", "SAMSUNG MZ PM9A3 960GB"),
            PlpStatus::Yes,
            "Samsung PM9A3 model strings should be recognised"
        );
        assert_eq!(
            lookup_table("MICRON", "7450 PRO 960GB"),
            PlpStatus::Yes,
            "Micron 7450 should be recognised"
        );
    }

    #[test]
    fn lookup_table_case_insensitive() {
        assert_eq!(lookup_table("intel", "ssdsc2kb"), PlpStatus::Yes);
        assert_eq!(lookup_table("INTEL", "ssdsc2kb"), PlpStatus::Yes);
        assert_eq!(lookup_table("Intel", "SSDSC2KB"), PlpStatus::Yes);
    }

    #[test]
    fn lookup_table_misses_consumer_drives() {
        // Consumer NVMe drives — known to NOT have PLP.
        assert_eq!(
            lookup_table("Samsung", "980 PRO 1TB"),
            PlpStatus::Unknown,
            "consumer 980 Pro must NOT match"
        );
        assert_eq!(
            lookup_table("WD", "BLACK SN770 1TB"),
            PlpStatus::Unknown,
            "consumer SN770 must NOT match"
        );
        assert_eq!(
            lookup_table("CRUCIAL", "P5 PLUS 1TB"),
            PlpStatus::Unknown,
            "Crucial P5 Plus consumer SSD must NOT match"
        );
    }

    #[test]
    fn lookup_table_misses_empty_strings() {
        assert_eq!(lookup_table("", ""), PlpStatus::Unknown);
        assert_eq!(lookup_table("INTEL", ""), PlpStatus::Unknown);
        assert_eq!(lookup_table("", "SSDSC2KB"), PlpStatus::Unknown);
    }
}
