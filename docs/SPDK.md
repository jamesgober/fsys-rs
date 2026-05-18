# SPDK backend for fsys (1.1.0)

> Status: **1.1.0 ships the gating + capability cache + observability
> surface.** The actual SPDK backend implementation lives in the
> companion crate `fsys-spdk`, which is in scaffold state as of
> 1.1.0 and ships in follow-up `1.1.x` releases. This document
> describes the full user-facing surface so consumers can write
> forward-compatible code against the 1.x stable API today.

## What SPDK is

SPDK (Storage Performance Development Kit) is a user-space NVMe
driver framework. Instead of going through the kernel's block
layer — `read()` / `write()` syscalls, page-cache buffering, the
NVMe driver's interrupt-driven completion path — SPDK opens the
NVMe controller directly from user-space and uses polling threads
to drain completion queues. The savings:

| Cost | Kernel + io_uring | SPDK |
|---|---|---|
| Syscall to submit IO | ~1.5 µs | 0 (no syscall) |
| Kernel block layer | ~3-5 µs | bypassed |
| Interrupt completion | ~10 µs tail | replaced by polling |
| Page cache mediation | always | bypassed |

For a write-ahead-log workload, the gross effect is **2-3× lower
commit latency** and **2-4× higher IOPS** on the same hardware.

## When to use SPDK

SPDK is the right backend when:

- You are running on a **dedicated Linux server** with at least
  4 cores and enterprise / data-centre NVMe hardware.
- Your workload is **latency-sensitive** — sub-10 µs commit
  latencies matter (database WAL, low-latency queue, transaction
  log).
- You can **dedicate the NVMe controller** to your process. SPDK
  takes exclusive ownership of the device; the kernel can no
  longer access it.

SPDK is the wrong backend when:

- You're on macOS or Windows. SPDK is Linux-only and will not be
  ported.
- You're on a **shared host** where the NVMe controller serves
  other processes via the kernel `nvme` driver. SPDK requires
  rebinding the device to `vfio-pci` / `uio_pci_generic`.
- Your **core count is under 4**. The polling-thread overhead
  exceeds the syscall savings on smaller hosts.
- You need **hot-pluggable hardware**, **mixed-workload
  isolation**, or **cgroup-style resource limits**. The kernel
  block layer provides these; SPDK does not.

For consumer / desktop / laptop hardware, the kernel + io_uring
path that ships in `1.0.0` is faster after factoring in setup
overhead.

## Hardware requirements

| Component | Requirement |
|---|---|
| OS | Linux ≥ 5.4 (`io_uring` parity), ≥ 5.19 recommended for NVMe passthrough features SPDK can use. |
| CPU | ≥ 4 cores. ≥ 8 cores recommended. |
| RAM | At least 1 GiB free to allocate as hugepages. |
| Storage | NVMe SSD (PCIe). SATA SSDs and HDDs are not supported. |
| BIOS / firmware | IOMMU must be enabled (`Intel VT-d` or `AMD-Vi` in BIOS), and the kernel must be booted with `intel_iommu=on` / `amd_iommu=on`. |

## System setup

The capability probe will tell you exactly which preconditions
are missing. The `SpdkSkipReason` returned in
`Error::SpdkUnavailable` names the failure; the remediation is in
the matching section below.

### 1. Allocate hugepages

```bash
# Allocate 1024 × 2 MiB hugepages = 2 GiB of hugepage memory.
sudo sysctl -w vm.nr_hugepages=1024

# Persist across reboot.
echo 'vm.nr_hugepages = 1024' | sudo tee -a /etc/sysctl.d/10-fsys-spdk.conf
```

Verify:

```bash
$ grep -E '^HugePages|^Hugepagesize' /proc/meminfo
HugePages_Total:    1024
HugePages_Free:     1024
Hugepagesize:       2048 kB
```

### 2. Enable IOMMU

Add to your kernel command line (`/etc/default/grub`):

```
GRUB_CMDLINE_LINUX_DEFAULT="... intel_iommu=on iommu=pt"
# (or amd_iommu=on on AMD systems)
```

Rebuild grub config, reboot, verify:

```bash
$ ls /sys/kernel/iommu_groups | head
0
1
2
...
```

### 3. Bind the NVMe device to vfio-pci

First, identify your NVMe controller:

```bash
$ lspci -nn | grep -i nvme
81:00.0 Non-Volatile memory controller [0108]: Samsung Electronics ...
```

Take the PCI address (`0000:81:00.0` in canonical form) and the
vendor:device ID pair (`144d:a808` for example).

Unbind from `nvme`, bind to `vfio-pci`:

```bash
# Replace 0000:81:00.0 and 144d:a808 with your values.
echo '0000:81:00.0' | sudo tee /sys/bus/pci/devices/0000:81:00.0/driver/unbind
echo '144d a808' | sudo tee /sys/bus/pci/drivers/vfio-pci/new_id
```

Verify:

```bash
$ ls -l /sys/bus/pci/devices/0000:81:00.0/driver
... -> ../../../../bus/pci/drivers/vfio-pci
```

### 4. Grant the application privileges

SPDK requires `uid 0` or `CAP_SYS_ADMIN`. The clean way is a
systemd unit with the capability ambient-set:

```ini
# /etc/systemd/system/myapp.service
[Service]
ExecStart=/usr/local/bin/myapp
AmbientCapabilities=CAP_SYS_ADMIN
```

For interactive use you can `sudo setcap cap_sys_admin+ep
/path/to/binary` once per binary.

### 5. Verify with the capability probe

```rust
let caps = fsys::capability::capabilities();
if caps.spdk_eligible {
    println!("SPDK is ready on this host.");
    println!("Eligible devices: {:?}", caps.spdk_eligible_devices);
} else {
    println!("SPDK not ready:");
    for reason in &caps.spdk_skip_reasons {
        println!("  {reason}");
    }
}
```

Or force a re-probe after configuration changes:

```bash
$ FSYS_REPROBE=1 ./myapp
```

## Reading the capability probe output

The capability probe writes its result to a TOML file under
`$XDG_CACHE_HOME/fsys/capabilities.toml` (Linux/macOS) or
`%LOCALAPPDATA%\fsys\capabilities.toml` (Windows). The file is
safe to read with any TOML parser and stable for the 1.x line:

```toml
schema_version = 1
fsys_version = "1.1.0"
kernel_version = "6.8.0-generic"
os_target = "linux"
probed_at_unix_secs = 1738620000

[capabilities]
io_uring = true
io_uring_features = ["coop_taskrun", "single_issuer", "defer_taskrun"]
nvme_passthrough = true
direct_io = true
plp_detected = false
spdk_eligible = false
spdk_skip_reasons = ["hugepages_lt:0:1024"]
spdk_eligible_devices = []

[hardware]
drive_type = "nvme"
optimal_block_size = 4096
queue_depth = 1024
sector_size_logical = 512
sector_size_physical = 4096
```

External tooling (monitoring agents, deployment validators) can
consume this file directly without invoking the fsys binary.

## SpdkSkipReason: what each value means

### `NotLinux`

**Cause:** SPDK is Linux-only. Selecting `Method::Spdk` on macOS,
Windows, or any other OS returns this reason.

**Fix:** Use a kernel-path method instead. `Method::Auto` will
pick the best available primitive for your platform.

### `HugepagesNotConfigured { current_mb, recommended_mb }`

**Cause:** `HugePages_Total * Hugepagesize` from `/proc/meminfo`
is below the 256 MiB eligibility floor. SPDK requires hugepage-
backed memory for its DMA buffer pool.

**Fix:** Section 1 above. Allocate at least 1024 MiB (`vm.nr_hugepages = 512`
for 2 MiB pages); the recommended floor for production workloads
is 1024 MiB.

### `InsufficientPrivileges`

**Cause:** the calling process lacks `uid 0` and `CAP_SYS_ADMIN`.
SPDK opens character devices in `/dev/vfio/` and pins hugepages;
both require privilege.

**Fix:** Section 4 above. Run as root (rare in production), grant
`CAP_SYS_ADMIN` via `setcap`, or configure a systemd unit with
`AmbientCapabilities=CAP_SYS_ADMIN`.

### `NoNvmeDevices`

**Cause:** No NVMe controllers were found on the PCI bus. The
probe walks `/sys/bus/pci/devices/` looking for `class=0x010802`
entries; none matched.

**Fix:** Verify with `lspci -nn | grep -i nvme`. If your hardware
has an NVMe controller but the probe doesn't see it, the device
class encoding may be non-standard — file an issue with `lspci
-nnvvv` output for the device.

### `AllDevicesInUse { devices }`

**Cause:** NVMe controllers were found but every one is exclusively
bound to the kernel `nvme` driver. SPDK cannot share access — it
requires either `vfio-pci` or `uio_pci_generic` binding.

**Fix:** Section 3 above. Rebind one or more devices. If the
system has multiple NVMe controllers, you can leave at least one
on the kernel driver for `/`-mount duties and rebind the rest
for SPDK.

### `IommuNotConfigured`

**Cause:** `/sys/kernel/iommu_groups/` is missing or empty. The
preferred `vfio-pci` SPDK driver requires IOMMU mediation; without
it, only the less-safe `uio_pci_generic` is available.

**Fix:** Section 2 above. Enable `intel_iommu=on` / `amd_iommu=on`
on the kernel command line + reboot. If your platform doesn't
support IOMMU at all (very old hardware), you can run SPDK on
`uio_pci_generic` exclusively — see the SPDK documentation for
that path; fsys will surface this reason but the SPDK backend
may still work depending on configuration.

### `InsufficientCores { available, recommended }`

**Cause:** the host has fewer than 4 cores available to the
process. SPDK's polling-thread architecture realistically needs
4+ cores: one for the runtime + workload, 2-3 for polling.

**Fix:** Use a larger host. On smaller hosts, the kernel +
io_uring path is faster overall because the polling overhead
dominates the syscall savings.

### `SpdkLibraryNotFound`

**Cause:** the `spdk` Cargo feature was enabled at compile time
but the runtime `libspdk` / `librte_*` shared libraries are
missing from the dynamic loader search path.

**Fix:** Install SPDK from packages (`apt install libspdk-dev`
on Debian/Ubuntu, distribution-specific elsewhere) or from
source. Run `ldconfig` after install to refresh the loader
cache.

## Configuration

```rust
use fsys::{Builder, Method};

let fs = Builder::new()
    .method(Method::Spdk)
    .spdk_device("0000:81:00.0")  // optional — defaults to first eligible
    .spdk_queue_depth(256)         // default
    .spdk_polling_threads(0)       // 0 = "use capability default"
    .spdk_hugepage_size_mb(1024)   // default
    .build()?;
```

### Tuning knobs

- **`spdk_device`** — pin to a specific PCI address. Default
  behaviour: the SPDK backend picks from
  `capabilities.spdk_eligible_devices`. Override when the host
  has multiple eligible devices and you want deterministic
  pinning.
- **`spdk_queue_depth`** — per-namespace NVMe submission queue
  depth. Default 256 (matches `Workload::Database`). Range
  64-1024 in practice; throughput scales linearly with queue
  depth up to the drive's saturation point.
- **`spdk_polling_threads`** — number of dedicated polling
  threads. `0` (default) lets the probe pick `cores / 4` clamped
  to `[1, 8]`. Smaller values reduce CPU overhead; larger values
  improve completion-latency tails under burst load.
- **`spdk_hugepage_size_mb`** — hugepage allocation for the DMA
  buffer pool. Default 1024 MiB. The minimum-viable floor is
  256 MiB; the recommended floor is 1024 MiB for sustained
  workloads.

## Observability

Every `JournalHandle` exposes three accessors for verifying which
backend is live:

```rust
let log = fs.journal("/var/lib/myapp/log.wal")?;

match log.backend_kind() {
    fsys::JournalBackendKind::Spdk => {
        println!("SPDK backend live");
    }
    other => {
        println!("Kernel path: {other}");
    }
}

let health = log.backend_health();
println!("Queue depth: {}/{}", health.queue_depth_current, health.queue_depth_max);
println!("p99 latency: {} µs", health.p99_append_latency_us);

let info = log.backend_info();
println!("Selected: {} ({})", info.selected, info.selection_reason);
for (skipped, reason) in &info.fallbacks_skipped {
    println!("  Skipped {skipped}: {reason}");
}
```

The `backend_info()` selection trail is the canonical way for ops
teams to verify that SPDK is actually serving a journal — silently
falling through to the kernel path when SPDK was requested would
invalidate downstream performance expectations.

## Forced re-probing

The capability cache invalidates automatically on fsys-version,
kernel-version, schema-version, or 30-day-age changes. After a
system reconfiguration (just enabled IOMMU, just rebound a
device), force a re-probe on the next process start:

```bash
$ FSYS_REPROBE=1 ./myapp
```

Or delete the cache file directly:

```rust
fsys::capability::invalidate_capability_cache()?;
```

Or from the shell:

```bash
$ rm "$XDG_CACHE_HOME/fsys/capabilities.toml"
```

## Performance expectations

Target floors on a properly configured Linux server (modern NVMe,
1+ GiB hugepages, dedicated cores). These will be confirmed with
benchmark capture once the SPDK backend ships:

| Metric | Kernel + io_uring (1.0) | SPDK target |
|---|---|---|
| Single-record append p50 | < 12 µs | < 4 µs |
| Single-record append p99 | < 80 µs | < 20 µs |
| Single-record append p99.9 | < 500 µs | < 80 µs |
| Sustained throughput (single namespace) | 400K-600K IOPS | 1-2M IOPS |
| Group commit batch (1KB × 100) | < 200 µs | < 60 µs |
| Open + first append (cold start) | < 50 ms | < 200 ms |

SPDK pays for itself on the **steady-state hot path**. Cold-start
is slower because SPDK initialisation includes DPDK environment
setup, controller probing, and DMA buffer pool allocation.

## What's NOT supported

- **SPDK on macOS / Windows** — Linux-only, no plans to port.
- **RDMA** — SPDK can do RDMA, but fsys is local-storage focused.
  A future minor release may add it.
- **Multiple controllers per JournalHandle** — one controller per
  handle in 1.1.0. Multi-controller is a 1.x extension.
- **Persistent memory (PMEM / CXL.mem)** — separate work stream,
  not part of SPDK.
- **Hot-swap of devices** — fsys assumes the hardware config at
  process start is stable for the process lifetime.

## See also

- [API reference](../docs/API.md) — full `fsys::capability` and
  `fsys::journal::backend` API.
- [Stability commitment](./STABILITY-1.0.md) — what the 1.x line
  guarantees about these new types.
- [`.dev/DECISIONS-1.1.0.md`](../.dev/DECISIONS-1.1.0.md) —
  rationale + alternatives for the design choices captured during
  1.1 development.
- [Upstream SPDK docs](https://spdk.io/doc/) — for low-level
  configuration details beyond what fsys exposes.
