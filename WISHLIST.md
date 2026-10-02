# Future Roadmap & Design Proposals

This document tracks planned architectural enhancements and feature proposals that have undergone preliminary design analysis but remain unbuilt or uncommitted. Each proposal outlines the underlying problem, intended technical mechanism, implementation costs, and identified constraints.

---

## 1. Pre-Transfer Disk Capacity Safeguard

### Context
Autobahn currently stages files into temporary directories prior to renaming them into place. If a transfer exceeds available disk space on the target filesystem, write operations fail mid-cycle, prompting exponential backoff and retries. While data integrity is preserved via atomic renaming, repeated attempts consume all available disk space until the condition is resolved.

### Proposed Architecture
Before staging writes, the session computes total transfer size (`staged_bytes` derived from `file_sizes` in `src/session/mod.rs`). The controller evaluates this against target filesystem free space (via `statvfs`) plus a safety margin:

- **Safety Margin:** Minimum reserve equal to `max(5% of filesystem capacity, 1 GB)`.
- **Preemptive Halt (`SafetyHalt::DiskSpace`):** Halts the sync cycle before issuing writes if space is insufficient. Resumes automatically once sufficient storage becomes available.
- **Minimal Remote Overhead:** Rather than adding synchronous round-trips before staging, remote agents continuously attach available filesystem capacity to existing scan responses and keepalive messages. The controller uses the latest cached reading for transfers exceeding 64 MB.

### Limitations
- The measurement represents a point-in-time estimate; concurrent writes by external processes during a transfer cycle may still exhaust disk capacity.
- The check operates on the net transfer volume and does not credit space reclaimed by pending deletions scheduled in the same cycle.

---

## 2. Power-Aware Adaptive Directory Audits

### Context
Background filesystem watchers (`inotify`, `FSEvents`) provide real-time change notifications but are susceptible to dropped events under heavy kernel load. To ensure correctness, Autobahn executes a full directory walk every 120 seconds by default (extended to 10 minutes on battery when `power_saver_experimental = true`). On directory trees exceeding 500,000 files, full directory walks incur non-negligible CPU and battery consumption.

### Proposed Architecture
Dynamically calibrate the full-walk audit interval based on measured execution duration and target CPU utilization budgets:

- **Dynamic Budgeting:** Target a maximum CPU utilization limit (e.g., 1% on battery, 5% on AC). Compute the next audit delay from the wall-clock time of the preceding walk.
- **Interval Bounds:** Maintain a strict lower bound (e.g., 2 minutes) for small repositories and an upper ceiling (e.g., 15 minutes) to bound silent event loss.
- **Wake Triggers:** Initiate an immediate audit upon waking from system sleep, mitigating the risk of missed kernel events during sleep transitions.
- **Host-Local Power State:** Query system power state independently on each host (`IOPSCopyPowerSourcesInfo` on macOS, `/sys/class/power_supply` on Linux).

---

## 3. Official FreeBSD Platform Support

### Context
FreeBSD builds compile successfully from source and pass the standard test suite. However, official release pipelines do not publish FreeBSD binaries or bundle them into `autobahn-agents.tar.gz`.

### Implementation Requirements
- **Release Automation:** Incorporate a FreeBSD VM runner (`vmactions/freebsd-vm`) into `.github/workflows/release.yml` with a pinned Rust toolchain.
- **Installer Integration:** Update `scripts/install.sh` to recognize `freebsd-x86_64` and unpack precompiled agent binaries into `~/.autobahn/agents/`.
- **Atomic Rename Semantics:** FreeBSD lacks an atomic `RENAME_NOREPLACE` equivalent. File publication on FreeBSD must utilize standard rename sequences, accepting a narrow check-to-use window during concurrent creation.

---

## 4. Intel macOS Testing Coverage

### Context
Release workflows build both Apple Silicon (`aarch64-apple-darwin`) and Intel (`x86_64-apple-darwin`) binaries. However, CI test automation executes exclusively on Apple Silicon runners, leaving Intel macOS binaries compiled but without automated functional verification.

### Implementation Requirements
- Add a dedicated Intel runner workflow step or run `x86_64` binaries under Rosetta 2 emulation in the macOS CI matrix.
- Ensure test suites pass across both target architectures prior to signing and notarization.

---

## 5. Linux Desktop System Tray Support

### Context
The status tray interface (`apps/tray`) compiles behind the optional `tray` Cargo feature flag. While fully supported on macOS, Linux desktop execution lacks automated CI testing and packaging.

### Implementation Requirements
- **GTK Context Initialization:** Properly initialize GTK loop bindings on the primary UI thread in `src/tray.rs` when targeting Linux.
- **Desktop Environment Validation:** Verify status icon behaviors across GNOME (via AppIndicator) and KDE Plasma.
- **Packaging:** Provide a standalone binary build and desktop launcher descriptor (`.desktop`) for user autostart configuration.
