//! Fallback hardware probe for platforms outside Linux / macOS /
//! Windows.
//!
//! Returns the conservative defaults from `DriveInfo::default`,
//! `MemoryInfo::default`, and `CpuInfo::default`. The `cores_logical`
//! field is real (via [`std::thread::available_parallelism`]); the
//! rest is left at the safe defaults the 0.2.0 foundation established.

#![cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]

use crate::hardware::cpu::{CpuFeatures, CpuInfo};
use crate::hardware::drive::DriveInfo;
use crate::hardware::io_primitives::IoPrimitives;
use crate::hardware::memory::MemoryInfo;

pub(crate) fn probe_drive() -> DriveInfo {
    DriveInfo::default()
}

pub(crate) fn probe_memory() -> MemoryInfo {
    MemoryInfo::default()
}

pub(crate) fn probe_cpu() -> CpuInfo {
    let cores_logical = std::thread::available_parallelism()
        .map(|n| u32::try_from(n.get()).unwrap_or(u32::MAX))
        .unwrap_or(1);
    CpuInfo {
        cores_logical,
        cores_physical: cores_logical,
        features: CpuFeatures::empty(),
        cache_l1: 0,
        cache_l2: 0,
        cache_l3: 0,
    }
}

pub(crate) fn probe_io_primitives() -> IoPrimitives {
    IoPrimitives {
        io_uring: false,
        iocp: false,
        kqueue: cfg!(any(
            target_os = "freebsd",
            target_os = "netbsd",
            target_os = "openbsd",
            target_os = "dragonfly",
        )),
        nvme_passthrough: false,
        direct_io: false,
        mmap: cfg!(unix),
    }
}
