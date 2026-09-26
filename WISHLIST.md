# Wishlist

Things we would like autobahn to do, thought through but not built or not supported yet. Each says what it is for, how it would work, what it costs, and where it falls short, so picking one up starts from the design rather than from scratch.

## Halt before a sync fills a disk

**The problem.** Nothing checks free space today. A cycle that doesn't fit stages until a write fails, then errors and retries with backoff. Nothing is corrupted, because content is staged and renamed into place only when it is whole. But every retry fills the disk to the brim again, which starves everything else on that machine, and the person hears about it only as a generic `errored` after two minutes.

**The check.** Before staging, the session already knows how many bytes the cycle will write: the sizes of the files it is about to transfer (`staged_bytes`, from `file_sizes` in `src/session/mod.rs`). It compares that, plus a reserve, with the free space of the filesystem holding the destination's staging directory, and of its root when that is a different filesystem. If the transfer doesn't fit, it stops before writing anything:

> halted: beta needs 12.4 GB for this cycle and has 3.1 GB free (keeping 1 GB spare), so nothing was copied; free up space and it resumes on its own

- **A new `SafetyHalt` variant.** Like the missing-alpha halt, it clears on its own when space frees up, and its `alert_after` is a couple of minutes rather than immediate. A disk being freed while you watch is common.
- **The reserve** is the larger of 5% of the filesystem and 1 GB, so the machine is never left with nothing. It could become a setting later if anyone asks.
- **Both sides are checked**, since two-way cycles write to both.

**No latency on the edit path.** This is the part that has to be right:

- **Local free space** is one `statvfs` call, a few microseconds.
- **Remote free space must not cost a round trip.** Small files (under 64 KB) are sent before the destination answers the staging request, and a transition that sent everything goes out without waiting for that answer. That is what made an edit to a remote beta one round trip instead of three and a half. So the answer to the staging request is the wrong place for free space. Instead the agent *volunteers* it in answers it already sends — every scan answer and every "unchanged" answer — and the controller keeps the latest reading, a few seconds old at most.
- **The check runs only on cycles that could plausibly fill a disk:** more than 64 MB to stage, or more than the last reading's spare. An ordinary edit of a few kilobytes never waits on anything and compares against nothing but a number in memory.

**Cost.** About a day:

- the free-space reading on each side
- one field on two existing responses, which is a wire change and so rides on the next compatibility epoch bump rather than forcing one of its own
- the check and the halt
- tests, taking the reading through a seam a test can override, since a test cannot really fill a disk; a small `tmpfs` checks it live on Linux

**Where it falls short.**

- **The reading is a moment's estimate.** Another process writing to the same disk mid-cycle can still fill it. The check stops the predictable case — a large first sync, one huge file — and staging keeps the rest safe, as it does today.
- **All or nothing.** It halts the whole cycle rather than syncing what fits. That is simpler and matches the other halts; syncing what fits is possible later, but a partly synchronized tree is harder to reason about.
- **Deletions in the same cycle free space too**, but they are applied in the same pass as the writes, so the check doesn't count on them.

**Related, and separate: a sync that saturates the disk.** A large sync can keep a disk busy enough that the machine crawls. That calls for a limit, not a halt: a cap on transfer bandwidth, or on the scan and apply threads, set in the configuration. Also about a day.

## A full walk that costs what the tree can afford

**The problem.** Every session re-reads its whole tree every 120 seconds, whether or not anything changed (`FULL_SCAN_INTERVAL`, `src/endpoint/observer.rs`). Everyday syncing doesn't depend on it: an edit arrives because the watcher reports it. The walk is the backstop for changes the watcher never reports, and its interval is the longest such a change can go unnoticed. But its cost grows with the tree, and on a Mac it is not small.

Measured on 2026-09-24: macOS 26.5.1, Apple M4, on battery, a 160,000-file corpus with both sides local, 30 one-minute `powermetrics` samples with nothing else running:

| | CPU ms/s | Energy impact |
|---|---|---|
| mean | 55.2 | 76.5 |
| median | 51.9 | 64.7 |
| max | 123.5 | 163.0 |

The samples alternate, as a 120-second walk lands in every other 60-second window. The 15 minutes with a walk averaged 152.9 energy and about 110 CPU ms/s. The 15 without one averaged 0.1 and about 1. For scale, in the same samples `sentineld` averaged 504 and WindowServer 116, so a walking minute sits between them. The walk is the whole of autobahn's idle cost, and it is enough to show among a laptop's noticeable consumers.

A walk costs about 41 µs of CPU per file on that Mac, against about 7 µs on Linux (a 420k-file tree took about 3 CPU-seconds a walk on a c6i.8xlarge). Assuming the cost is linear in the tree's size, which matches the one measurement (it predicts 55 ms/s for 160k files), today's fixed interval costs:

| Files | Every 2 minutes, Mac |
|---|---|
| 5k | 1.7 ms/s |
| 50k | 17 ms/s |
| 160k | 55 ms/s |
| 500k | 171 ms/s |
| 1M | 342 ms/s, a third of a core |

So this is not only a battery problem: a very large tree is expensive on AC power too, all day.

**What we hope to resolve.** The walk should cost what the machine can afford, not what the tree's size dictates, without giving up the promise that a silent miss is always found.

- **The parameter is a CPU budget, and the interval follows from it.** Each walk is timed, and the next one waits long enough that walking uses at most, say, 1% of a core on battery and 5% on AC. The result is clamped between a floor and a ceiling.
- **A floor of 2 minutes,** today's interval, so small trees lose nothing: a 5k-file tree walks in a fraction of a second and keeps walking every 2 minutes.
- **A ceiling,** say 15 minutes, which is the promise about silent misses: no change the watcher missed goes unseen longer than that.
- **A walk on wake from sleep,** the most common moment for a laptop to miss events, so the longer interval matters less.
- **Settings for the whole machine, not per session,** since power belongs to the machine. Each process decides for its own host, so a remote agent on a plugged-in Linux box keeps its own pace while the laptop saves power. The power source is `IOPSCopyPowerSourcesInfo` on macOS and `/sys/class/power_supply` on Linux, cached for a minute.
- **Visible in status,** for example "power saver: on (battery), walking every 11m", so a longer ceiling on missed changes is never hidden.

At a 1% budget with those bounds, on the Mac:

| Files | Interval | Cost |
|---|---|---|
| 5k | 2 min (floor) | 1.7 ms/s |
| 50k | 3.4 min | 10 ms/s |
| 160k | 11 min | 10 ms/s |
| 220k | 15 min (ceiling) | 10 ms/s |
| 500k | 15 min | 23 ms/s |
| 1M | 15 min | 46 ms/s |

From about 30k to 220k files the cost is held at 1% of a core. Above the ceiling it climbs again, and the choice there is between a longer ceiling and a costlier walk. The configuration might look like:

```toml
[power_saver]
when = "battery"          # "battery" (default), "always", or "never"
walk_budget = "1%"        # of one core, while saving
walk_ceiling = "15m"
```

**What a longer interval risks.** A delay, not data. Every write is checked against the scan it was planned from, so a file that changed without an event cannot be overwritten blindly. The worst case is a stale copy on the other side for one interval, and perhaps a conflict to settle. Most misses aren't silent either: when FSEvents or inotify drops events it says so, and autobahn already walks at once. The periodic walk covers only what the operating system never reports.

**What we don't know: how often silent misses happen.** It depends on the person and the workload: sleep and wake, external or network volumes, builds writing thousands of files. No fixed number is right for everyone, which is why the budget sets the interval rather than a guess at the miss rate. A cheap first step is to have each full walk log how many changes it found that no event reported. A few days of that on real machines would say whether 15 minutes is cautious or loose, and whether some watcher gap is worth fixing outright.

**Cost.** About two days: the power-source check on each platform, timing the walk and choosing the next interval, the wake trigger, the configuration and its reload, the status line, and tests with the clock and power source behind seams. The miss-rate log is an hour on its own and could go first.

**Where it falls short.**

- **The per-file cost is one measurement,** on one Mac and one tree, and assumed linear. Trees with deep paths, many small directories, or a slow volume may cost more per file.
- **The walk's CPU is not the whole of its energy.** Waking the disk and the memory traffic count too, and `powermetrics` attributes them only roughly, so the budget is a proxy for energy.
- **A ceiling is still a ceiling.** On a million-file tree even 15 minutes costs 4.6% of a core. Only walking less, walking part of the tree at a time, or trusting the watcher more would go further, and each weakens the promise.

## FreeBSD as a supported platform

**The problem.** FreeBSD was half-supported. CI ran the full suite in a FreeBSD VM on every push, and the docs listed it beside Linux and macOS. But no release published a FreeBSD build, `install.sh` refused it, and the agent bundle had no FreeBSD binary, so a FreeBSD remote host could not be set up without building one by hand. Two update tests that assume every tested platform has a published build kept CI red. FreeBSD was dropped for now: the CI job is removed, and the docs no longer mention it.

**Building it yourself today.** It builds from source, and until September 2026 the full test suite passed on FreeBSD x86-64 in CI. Nothing tests it now, so it may drift. Someone who builds it gets:

- **No install or update.** `install.sh` refuses FreeBSD, and `autobahn update` has nothing to download. Every upgrade is a rebuild.
- **Remote hosts only with a hand-supplied agent.** The agent bundle has no FreeBSD build. The binary goes on the controller as `~/.autobahn/agents/autobahn-freebsd-x86_64`, or in a directory named by `AUTOBAHN_AGENTS_DIR`. It must come from the same release tag as the controller, or the version check refuses it. Without one, the session reports the host as unreachable and retries indefinitely (review item L-21).
- **Weaker file creation.** FreeBSD has no atomic "create only if absent" rename, so `publish_rename` falls back to a plain rename. In the small window between check and rename, a file someone else creates at the same moment can be replaced.
- **x86-64 only.** FreeBSD on arm64 has never been tested, and other BSDs have never been tried.

**What it would take.**

- **A release build.** Build `autobahn-freebsd-x86_64` in the release workflow, inside the same kind of VM the old CI job used (`vmactions/freebsd-vm`), pinned to a commit rather than a tag. It runs in parallel with the other builds.
- **Installer and bundle.** Accept `freebsd-x86_64` in `install.sh`, and include the binary in `autobahn-agents.tar.gz` so the controller can bootstrap FreeBSD hosts.
- **CI back.** Restore the FreeBSD test job. It was also the only test of the plain-rename fallback in `publish_rename`, which every platform without an atomic no-replace rename uses.
- **A pinned toolchain.** The old job installed Rust from FreeBSD packages, whatever version was current that day. A release build should install a fixed version with `rustup` inside the VM, as the other builds do.
- **A version policy.** Linux builds link statically with musl and run on any Linux. A FreeBSD build links against the system's C library, so a binary built on FreeBSD 14 may not run on 13. Pick the oldest supported release, build on it, and document it.

**Cost.** Nothing in money, because the repository is public and the VM runs on a free Linux runner. About 4 minutes of extra wall-clock per release: the old CI job took 5.5 to 6 minutes including tests, against 2.3 minutes for the slowest existing release build. About a day of work, plus keeping the VM action and the toolchain pin current.

**Where it falls short.**

- **x86-64 only.** FreeBSD on arm64 would need an emulated VM, which is much slower.
- **Creation is still weaker than on Linux and macOS.** FreeBSD has no atomic no-replace rename, so creation keeps the check-then-use window described in `docs/safety.md`. Shipping builds makes that window a supported behavior rather than a curiosity.
- **Other BSDs stay out.** OpenBSD and NetBSD have never been tried, and each would be its own entry.

**A cheaper step first.** Keep a FreeBSD CI job that runs only by hand (`workflow_dispatch`), so drift is caught before anyone relies on a self-built binary. This costs nothing on normal pushes.

## Intel Macs as a tested platform

**The problem.** The release builds and publishes `autobahn-darwin-x86_64`, and `install.sh` accepts it, but no CI job runs on an Intel Mac. The macOS job runs on an Apple Silicon runner, and the release cross-compiles the Intel binary there. The Intel build ships but has never been tested, and the docs name Apple Silicon only. The menu bar app is Apple Silicon only.

**What it would take.**

- **A test job on an Intel runner.** Run the suite on GitHub's Intel macOS image, alongside the existing macOS job and gated the same way. It is free for a public repository, but macOS runners are scarce and slow to start, and GitHub has been retiring its Intel images, so check what is still offered.
- **Or test under Rosetta.** Run the x86-64 test binary on the Apple Silicon runner through Rosetta 2. That catches build and logic errors, but not everything a real Intel machine would, such as timing and performance differences.
- **Docs.** Once tested, list Intel Macs beside Apple Silicon.

**Cost.** Nothing in money. One more macOS job per push, or a Rosetta step in the existing job, and under an hour of work.

**Where it falls short.**

- **Intel runners may not last.** If GitHub drops its Intel images, Rosetta is the only option left, and Apple is phasing Rosetta out as well.
- **The app stays Apple Silicon only.** This covers the command-line binary.

**If it isn't done.** The alternative is to stop publishing the Intel build, so nothing untested ships. Until one or the other happens, the Intel binary is published but untested.

## A supported Linux tray

**The problem.** The tray code is shared with macOS and compiles behind the `tray` feature, but on Linux nothing builds it. No release includes it and CI never compiles it. `docs/macos-app.md` has an unverified build-it-yourself section, and nobody has confirmed that section's steps work.

**What it would take.**

- **Make it start.** On Linux the tray libraries need GTK initialized on the thread that runs the event loop, and `src/tray.rs` never does this, so a build may show no icon. Initialize GTK there under `cfg(target_os = "linux")`, and check that `winit`'s event loop and GTK's can share the thread, or run the tray on GTK's loop instead.
- **Verify it once by hand.** Build and run it on a common desktop, such as Ubuntu with GNOME plus the AppIndicator extension, and on KDE. Then pin down the exact package list and take "unverified" off the guide.
- **CI.** Add the GTK development packages to the Linux job, then lint and test with `--features tray`. That adds about a minute to each push.
- **A release artifact, or not.** Publish a separate Linux tray binary, or keep it build-it-yourself with CI coverage. A separate binary keeps GTK out of the command-line tool and the agent.
- **Autostart.** Ship a `.desktop` file for the autostart directory, the Linux counterpart of the macOS Login Items step.

**Cost.** About a day to make it start and verify it by hand, plus the CI minute. Nothing in money.

**Where it falls short.**

- **The dependencies.** It adds about 170 crates, mostly GTK 3 bindings, which are no longer maintained. `glib 0.18` has a soundness advisory. Supporting it means accepting those until the tray libraries move to GTK 4 or away from GTK.
- **Tray support varies by desktop.** GNOME shows no tray icons without an extension, so it cannot work the same everywhere.
- **Testing stops at the build.** CI can compile and unit-test it, but cannot check that an icon appears on a real desktop.

## Replace bincode 1.x

**The problem.** Every wire message, every ancestor journal and checkpoint, and every scan cache is encoded with `bincode` 1.3.3. That line is no longer maintained (RUSTSEC-2025-0141). Nothing is known to be wrong with it: decoding from a slice checks declared lengths against the input, and serde limits how much it allocates up front. But it will get no further fixes, and it sits on the path that decodes everything a peer sends.

**What it would take.**

- **Pick the successor.** `bincode` 2 is the closest match. `postcard` is smaller and designed for untrusted input. Either should decode with an explicit size limit.
- **Change the wire format together with the next compatibility-epoch bump** that happens for other reasons. The handshake already requires both ends to run the exact same version, so the switch lands in one release with no mixed-version period.
- **Migrate state on disk.** Ancestor checkpoints and journals must still open after the upgrade. Read the old format once, write the new one, and keep reading the old format for a release or two. Scan caches can simply be dropped and rebuilt.
- **Consider a depth limit** while the decoders are being replaced. Tier 2 treats deep nesting from a hostile peer as out of scope (I9-B), but it would be nearly free here.

**Cost.** About two days: the encoding change, the state migration and its tests, and a round trip through the existing crash-cut and journal-cut harnesses.

**Where it falls short.**

- **One-way upgrade.** The first release on the new format can't talk to older agents, which is already true of every epoch bump. The state migration can't be undone by downgrading.
- **No behaviour change.** It retires a maintenance risk. It adds no protection against a hostile peer beyond the explicit limit.
