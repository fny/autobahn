# Mac bench

What can only be checked on a Mac, and how. Everything here was built and tested on Linux. On macOS the watcher is FSEvents rather than inotify, the menu bar app compiles at all, drives live under `/Volumes`, and a laptop sleeps and runs on battery.

Build first, from `main` at or after `dd4f7d3` (compatibility epoch 14):

```sh
cargo build --release
apps/macos/build.sh          # the menu bar app; also replaces target/release/autobahn for the login service
autobahn restart             # if a login service is installed
```

Write results under each section's **Record** as you go, with the commit you tested.

## 1. The menu bar app compiles, and shows the two new lines

`src/tray.rs` is macOS-only and was changed twice without ever being compiled: a line and a notification for a refused configuration edit, and a line for a supervisor of another build. `apps/macos/build.sh` succeeding is the first half of this check.

**Refused configuration.** With the service running and the app open:

1. Add a bad key to `~/.autobahn/config.toml`, e.g. `mdoe = "two-way-conflict"` at the top.
2. Within about four seconds: one notification, *configuration refused: …*, and a menu line *⚠ configuration refused: …* naming `mdoe`. The summary line says *configuration refused*. Sessions keep syncing, so edit a file and watch it arrive.
3. Leave it broken for a minute. There should be no second notification.
4. Remove the key. Within about four seconds the menu line is gone, and `autobahn status` shows no refusal.

**Another build.** The app and the CLI talk to the supervisor through `control.sock` and now send their build with every request.

1. `autobahn stop`, then run an older build's supervisor in a terminal: `~/.local/bin/autobahn.previous watch --log`, or any 0.4.0 release binary.
2. Open the app (the current build). Expect *restart needed* in the summary and a menu line *⚠ the running supervisor is … and this is …*, or, since a pre-versioning supervisor can't answer, *a supervisor is running but does not understand this build*. It must not read as *Not running*.
3. `autobahn status` and `autobahn mi` say the same.
4. Stop the old one and `autobahn start`: the line goes.

**Record:** did it build; screenshots of both lines; anything that read as *Not running* or *errored* when it shouldn't.

## 2. What the two-minute full walk costs on battery

A running session walks both trees in full every 120 seconds (`FULL_SCAN_INTERVAL`, `src/endpoint/observer.rs`), because events can be lost. Since the walk went parallel, each one is a short burst on up to eight threads. On a build box nobody cares; on a laptop on battery it may matter. It isn't configurable, so this compares two builds.

1. A tree the size of a real one: `python3 bench/corpus.py code ~/bench-160k/a --scale 4` (160,000 files), then `cp -R ~/bench-160k/a ~/bench-160k/b`.
2. A config with just that group, `two-way-conflict`, both sides local.
3. Unplug the charger. Leave the machine idle: no edits, lid open, display may sleep.
4. For 30 minutes, run `sudo powermetrics --samplers tasks --show-process-energy -i 60000 -n 30 > ~/walk-120.txt` and keep `autobahn watch --log` running.
5. Build a variant with the walk every ten minutes: set `FULL_SCAN_INTERVAL` to `Duration::from_secs(600)`, `cargo build --release`, and repeat step 4 into `~/walk-600.txt`.
6. Compare autobahn's rows: average *energy impact*, and CPU ms/s.

**Decides:** if the 120-second build's energy impact is well above the 600-second build's, and high enough to show up among the Mac's usual top consumers (Activity Monitor → Energy, 12-hour column), make the full walk longer on battery. If the two are close, the walk isn't the idle cost and the item closes.

**Record:** both averages, the macOS version, and the machine.

## 3. The Linux wins, measured on macOS

Three changes were measured only on Linux:

- both endpoints watched at once (p90 125 → 52 ms)
- a standing watch standing in for a scan
- the 25/5 ms settle (one editor p50 47 → 25 ms)

FSEvents coalesces and delivers on its own schedule, so the numbers may differ.

```sh
# A: a 0.4.0 release binary. B: this build.
bench/ab.sh ~/autobahn-0.4.0 target/release/autobahn --legs 5
bench/ab.sh ~/autobahn-0.4.0 target/release/autobahn --legs 5 --agents 10
bench/ab.sh ~/autobahn-0.4.0 target/release/autobahn --legs 5 --remote <linux box>
```

The first two are local on the Mac; the third puts the destination on the Linux box over ssh, which runs each leg's own binary as the agent there. Close other busy apps first; each leg is a cold sync plus a minute of editing.

**Decides:** if B's p50 and p90 are not clearly below A's locally on macOS, FSEvents' own latency is the floor there, and the settle window may be worth tuning per platform. If B wins as on Linux, nothing to do.

**Record:** the three summary blocks as printed.

## 4. `ignore_mounts` with a real volume

`ignore_mounts = true` (the default) leaves any directory on another device than its root alone: on both sides, and still when it's unplugged. On Linux a tmpfs mounted inside an alpha under `watch` behaved as intended. On macOS the watcher is FSEvents, which has none of the device check the Linux watcher now has, so events from inside the mount will arrive. They should cause nothing but a scan that finds nothing to do.

```sh
hdiutil create -size 50m -fs APFS -volname Mnt ~/mnt.dmg
mkdir -p ~/synced/a/mnt ~/synced/b/mnt
echo beta-own > ~/synced/b/mnt/own.txt
hdiutil attach -mountpoint ~/synced/a/mnt ~/mnt.dmg
echo in-the-mount > ~/synced/a/mnt/secret.txt
```

A group from `~/synced/a` to `~/synced/b`, `two-way-conflict`, run with `autobahn watch --log`:

1. **Mounted:** `secret.txt` never reaches `b`, and `b/mnt/own.txt` stays. `cat ~/.autobahn/sessions/*/mounts` lists `mnt` under alpha.
2. **Write inside the mount:** `echo more >> ~/synced/a/mnt/secret.txt`. Nothing is carried, no conflict, nothing in the log beyond a quiet cycle.
3. **Edit elsewhere in `a`:** it arrives as usual, and `mounts` still lists `mnt` (the incremental scan carries it).
4. **Unplug:** `hdiutil detach ~/synced/a/mnt`. Nothing moves; `b/mnt/own.txt` stays; no conflict. `mounts` still lists `mnt`.
5. **Replug:** nothing moves.
6. Set `ignore_mounts = false` on the group, then:
   - mounted: `secret.txt` syncs to `b` like any file
   - detach: the session halts with *mnt on alpha was a mount point and is now empty or gone*, and `b/mnt/secret.txt` is not deleted
   - reattach: it resumes

Also mount a volume *under* a `/Volumes/...` alpha, if there's a real external drive to try with.

**Record:** each step's outcome, and the log lines around steps 2, 4 and 6.

## 5. A missing alpha on wake

A group whose alpha is on an external drive, or a disk image attached at `/Volumes/…`. A missing alpha folder now reports `halted`, with a message saying why, and alerts only after two minutes, since a drive often comes back with the wake.

1. Service running, `on_alert` set (or none, so the app notifies).
2. `hdiutil detach` the volume, or unplug the drive. Within a cycle `autobahn status` shows `halted: the alpha folder … is missing, so nothing was synchronized …`. There should be no notification yet.
3. Reattach within two minutes: no notification at all, and it resumes on its own.
4. Detach again and wait three minutes: exactly one notification.
5. Close the lid with the drive attached and reopen it after ten minutes: nothing, or at most the `halted` line in status for a moment and no notification.

**Record:** what notified, and when.

## 6. Agent names across platforms

A remote agent is now named by its content, `~/.autobahn/bin/autobahn-<version>+e14-<digest>`. The remote command is a POSIX `sh` script that picks the build for the host's platform. Both only ran Linux to Linux.

**Mac to Linux:**

```sh
autobahn sync ~/tmp/x <linux box>:/tmp/x --mode two-way-conflict
ssh <linux box> 'ls -l ~/.autobahn/bin/'
autobahn sync ~/tmp/x <linux box>:/tmp/x --mode two-way-conflict   # uploads nothing
ssh <linux box> 'ls -l ~/.autobahn/bin/'
```

The first run uploads the Linux agent from `~/.autobahn/agents` (the release bundle, which now carries a `MANIFEST`). The name carries a digest; the second run adds nothing.

**Linux to Mac:** the reverse, from the Linux box to the Mac as the remote. The Mac's login shell is zsh, and the launcher is wrapped in `sh -c` so that doesn't matter; this confirms it. It also needs a darwin agent in the Linux box's bundle.

**A stale bundle:** in a copy of the bundle, edit `MANIFEST` so the Linux line names `0.3.9+e12`. Point `AUTOBAHN_AGENTS_DIR` at it, and delete the remote's agent so an upload is needed. The sync is refused before anything is sent, with *the agent bundle in … is for 0.3.9+e12*.

**Record:** the `ls` listings, and the refusal message.

## 7. Does a build in an ignored directory force full walks?

On Linux the watcher never watches an ignored directory, and the kernel merges repeated events for one file, so neither a build in an ignored `target/` nor a file written thousands of times fills the change record (measured: no full walk). FSEvents watches the whole tree and reports what the ignore set would have kept out, so on macOS a `cargo build` inside an ignored `target/` of a synced tree may fill the 8,192-path record and make the next cycle walk everything (TODO-SPEED, "The change record fills with noise").

1. A synced Rust project with `target` in its ignores, and `autobahn watch --debug`.
2. `cargo build` (a clean one, so it writes thousands of files), and count cycles in the log whose `cycle finished in` is as long as a full walk of the tree.
3. If they appear: the fix is to filter each FSEvents path through the ignore set before recording it, and to deduplicate, in `PendingChanges::record` (`src/endpoint/local.rs`).

**Record:** full walks during the build, and the tree's size.

