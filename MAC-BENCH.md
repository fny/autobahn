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

**Result — d7c2e21 (epoch 14), macOS 26.5.1, Apple M4, 2026-09-24.** Partly done; the two menu lines still need a person.

- **It builds.** `apps/macos/build.sh` succeeded and signed the bundle as *Developer ID Application: Faraz Yashar (9R9RTH4HE6)*. So `src/tray.rs` compiles with both changes in it.
- **Refused configuration, from the outside:** tested under `watch --log` in a throwaway `AUTOBAHN_HOME`, because the real service could not be restarted (see the note at the end of this file). Adding `mdoe = "two-way-conflict"` produced one log line, six seconds later — *configuration refused; the sessions keep running as before: … unknown field `mdoe`, expected one of `reload`, `on_alert`, `disabled_hosts`, `disabled`, `log`, `defaults`, `groups`, `advanced`, `alerts`*. A file written afterwards still reached the beta. After 90 seconds there was still exactly one refusal line. Removing the key logged *configuration reloaded from …* and the session went on.
- **Gap: `autobahn status` cannot be used while the configuration is refused.** It parses the file itself and exits with the TOML error, so the one moment a person most wants to see their sessions, the CLI shows nothing at all — while the supervisor is fine and the app is expected to show a line. Worth its own item: `status` should fall back to the recorded status files, as the supervisor falls back to the last good configuration.
- **Not done:** the notification and the two menu lines (a person has to see them), and the *another build* half — it needs the app watching the default state root, which is blocked below.

**Filed as** [`MAC-2`](REVIEWS/fixes/MAC-2-status-during-refused-config.md) (L-41) for the `status` gap.


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

**Not run.** It needs the charger out for two 30-minute idle windows and `sudo powermetrics`; the machine was on AC at 25%. Left for a person.


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

**A side:** there is no 0.4.0 release binary to download — the only release with assets is v0.1.0, since the v0.4.0 run failed before its release job. A was built from the `v0.4.0` tag instead, which is the same code the release would have carried.

**Not run, and here is the obstacle.** `bench/ab.sh` drives each binary with `autobahn watch --config … --state-root …` (`bench/ab.sh:125`), so the A side has to be a build that has `watch`. Three candidates all failed:

- **A published 0.4.0 binary** does not exist. The only release carrying assets is `v0.1.0`; the `v0.4.0` run failed before its release job.
- **The `v0.4.0` tag** builds, but that code has no `watch` subcommand at all — `error: unrecognized subcommand 'watch'`.
- **The commits just before the watch work** (`51f4325^` = `f68b0a8`, and `c4e4109`) do not compile: *error[E0004]: non-exhaustive patterns: `Command::Update { .. }` not covered*, `src/main.rs:547`. The `Update` variant was added to the enum before the arm that handles it, so several commits in that range have never built. Worth knowing on its own, separately from this bench.

Picking an A therefore needs a walk back through that range for the first commit that both builds and has `watch`. Left undone rather than done against an invalid baseline.

**Filed as** [`MAC-4`](REVIEWS/fixes/MAC-4-non-building-commits.md) (I-6) for the commits that do not build.


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

**Result — d7c2e21 (epoch 14), macOS 26.5.1, Apple M4, 2026-09-24.** Steps 1 to 5 as written. Step 6 does not halt.

A 50 MB APFS image attached at `~/mb-synced/a/mnt`, group `~/mb-synced/a` → `~/mb-synced/b`, `two-way-conflict`, under `watch --log`.

1. **Mounted:** `secret.txt` never reached `b`; `b/mnt/own.txt` stayed; `plain.txt` synced normally. `mounts` read `{"alpha":["mnt"],"beta":[]}`. ✓
2. **Write inside the mount:** nothing carried, no conflict, and no log line at all for it. ✓
3. **Edit elsewhere:** `plain2.txt` arrived; `mounts` still listed `mnt`. ✓
4. **Detach:** nothing moved, `b/mnt/own.txt` stayed, no conflict, `mounts` still listed `mnt`. ✓
5. **Replug:** nothing moved. ✓
6. **`ignore_mounts = false`:**
   - The running supervisor picked the edit up by itself — *configuration reloaded from …* — and then synced `secret.txt` to `b`. ✓
   - **Detach: no halt.** `status` stayed *synchronized*, 22 cycles after a forced flush, `error: null`, no conflicts, no blocked paths — while `a/mnt` was empty and `b/mnt` still held `own.txt` and `secret.txt`. Nothing was deleted, which is the important half, but the state word says the two sides agree when they do not, and no message names the mount. Expected here: *mnt on alpha was a mount point and is now empty or gone*.
   - **Reattach:** resumed, and the volume was repopulated from `b` (`own.txt` and `secret.txt` on both sides). ✓
   - Likely cause of the non-halt: the `mounts` record still lists `mnt` under alpha after the flag is turned off, so the boundary protection still applies while the state word comes from somewhere that no longer knows about it.

**Filed as** [`MAC-1`](REVIEWS/fixes/MAC-1-detached-mount-not-halted.md) (M-59).

No external drive was available, so the `/Volumes/...` variant was not tried.


## 5. A missing alpha on wake

A group whose alpha is on an external drive, or a disk image attached at `/Volumes/…`. A missing alpha folder now reports `halted`, with a message saying why, and alerts only after two minutes, since a drive often comes back with the wake.

1. Service running, `on_alert` set (or none, so the app notifies).
2. `hdiutil detach` the volume, or unplug the drive. Within a cycle `autobahn status` shows `halted: the alpha folder … is missing, so nothing was synchronized …`. There should be no notification yet.
3. Reattach within two minutes: no notification at all, and it resumes on its own.
4. Detach again and wait three minutes: exactly one notification.
5. Close the lid with the drive attached and reopen it after ten minutes: nothing, or at most the `halted` line in status for a moment and no notification.

**Record:** what notified, and when.

**Result — d7c2e21 (epoch 14), macOS 26.5.1, Apple M4, 2026-09-24.** Halts and self-clears as described; the two-minute claim does not hold.

A 20 MB image attached at `~/mb5/vol` as the alpha, `on_alert` writing a timestamped line to a file.

2. **Detach at 00:08:55:** within one cycle, `status` showed **halted** — *one side's synchronization root was emptied; propagate the deletion manually or restore the content, then run again*. No notification. Note the wording: `hdiutil detach` leaves the mount point behind as an empty directory, so this is the emptied-root halt, not the *alpha folder is missing* one the section describes.
3. **Reattach at 00:09:28 (33 s later):** no notification, and the session resumed by itself — *synchronized*, both sides holding `file.txt`. Worth knowing that this contradicts `docs/safety.md:58`, which says a halt needs a person and retrying never clears it.
4. **Detach again at 00:10:04:** exactly one notification, at **00:11:05 — 61 seconds later**, not two minutes. `built_in_after` in `src/config.rs:233` still reads `Alert::Halted => Duration::ZERO`; the minute that passed is `DEFAULT_COALESCE_AFTER`, not a hold. So a drive that goes away pages about a minute later, and the "often comes back with the wake" reasoning is not implemented.
5. **Lid closed for ten minutes:** not done, needs a person.

**Filed as** [`MAC-3`](REVIEWS/fixes/MAC-3-halted-alert-timing.md) (L-42), covering both the alert timing and the `docs/safety.md` contradiction.


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

**Result — d7c2e21 (epoch 14), macOS 26.5.1, Apple M4, 2026-09-24.** Mac → Linux (fny.voltai.party, x86-64) as written.

- **First sync** uploaded `~/.autobahn/bin/autobahn-0.4.0+e14-363a5902eae7`, 6,643,928 bytes. The digest in the name is the first 12 hex of the bundle binary's blake3, which I computed independently: `363a5902eae7ca03a99fd50eac6ff7411d565464f01d3f59d291e4f2632680e2`. The file synced, and `/tmp/x/a.txt` read back on the far side.
- **Second sync** added nothing: still exactly one `e14` agent on the host. ✓
- **Stale bundle, with a manifest:** `MANIFEST` edited to say `0.3.9+e12`, the remote agent deleted. Refused before anything was sent — *the agent bundle in /Users/faraz/mb6-home/agents is for 0.3.9+e12, and this is 0.4.0+e14; `autobahn update` installs the matching bundle* — and the host received nothing. ✓
- **Stale bundle, without a manifest** (a hand cross-build, which is what `~/.autobahn/agents` holds today): not caught up front, as designed, and the handshake caught it with a message that names the bundle and its age rather than blaming the host: *the agent just installed on ubuntu@fny.voltai.party does not match this build. The linux-x86_64 bundle it was copied from is stale: … Rebuild it, or remove it so a matching one is used.* ✓
- **Linux → Mac:** not run. The bundle on fny has no darwin agent, so it would test the missing-agent path rather than the launcher.


## 7. Checks for the review tickets

The 2026-09-23 reviews produced tickets in `REVIEWS/fixes/`. These are the parts only a Mac can check, because FSEvents, APFS, `osascript`, the menu bar app, signing, or macOS's handling of `sudo` and hard links differ from Linux. Run each one after its ticket lands. Items marked **Before the fix** can also be run today, to confirm the bug on macOS. Use throwaway folders and a throwaway state root (`--state-root ~/mb7/state`) for all of them.

**Filing a ticket from a result.** When a check fails, or finds something new, file a ticket so it isn't lost in this file:

- **Where.** A new Markdown file in `REVIEWS/fixes/`.
  - If the check belongs to an existing ticket, which each heading here names, add the result to that ticket instead of starting a new one.
  - Otherwise name the file with a new ID and a short slug. Use `MAC-<n>-<slug>.md` for something only the Mac found, and `F-<review ID>-<slug>.md` for a review item that has no ticket yet.
- **What goes in it.** Use the shape the existing tickets use:
  - a title saying what should be true, such as "An emptied root with a `.DS_Store` still halts";
  - **Findings**, the review IDs it covers or "new, from MAC-BENCH 7x", and **Status**, usually "proposed", with a severity;
  - **Problem**, with file and line references where you have them. Paste this section's **Record** text in as the evidence, with the commit, the macOS version and the machine;
  - **Proposed resolution**, which can be a sentence if the fix isn't clear yet;
  - **Tests**, including the steps from here that reproduce it.
- **Link it.**
  - In `REVIEWS/FINAL.md`, add a dated resolution line to the matching entry, pointing to the ticket.
  - A new finding gets a new entry in its severity section, numbered after the last one there. Raise that section's count in the table at the top.
  - Back here, add "Filed as `<ticket>`" under the check's **Record**.

### 7a. An emptied root with a `.DS_Store` left in it — F-C1

Section 5 found that detaching a volume leaves an empty mount point, and the emptied-root halt catches it. Finder often leaves a `.DS_Store` in that folder, and C-1 says the halt then misses.

1. Attach a small disk image as the alpha, as in section 5, with 20 files, and sync it to a local beta.
2. Detach it, then `touch <mount point>/.DS_Store`.
3. Run one cycle.

**Expected after the fix:** `halted`, and the beta keeps all 20 files. **Before the fix:** the beta loses all 20 files, in every mode, including `two-way-paranoid`.

**Note (2026-09-24, from reading the code):** being *ignored* does not protect a leftover. `one_side_emptied_root` (`src/session/mod.rs:1335`) judges a side gone by `children().is_empty()`, and `src/scan/mod.rs:730` records an ignored entry as an `Untracked` child — so the shipped default ignore of `.DS_Store` still leaves the root non-empty. The check is really "any leftover entry, ignored or not". Recorded on the C-1 entry in `FINAL.md`.

**Filed against** C-1, which now carries this reproduction.

**Record:** the status line, and the beta's file count.

**Result — `d7c2e21` plus fny's uncommitted peering rename, macOS 26.5.1, Apple M4, 2026-09-24. Reproduced, in both modes.**

20 files on a 20 MB APFS image at `~/mb7/vol`, synced to `~/mb7/b`, state root `~/mb7/state`.

| mode | after `hdiutil detach` + `touch .DS_Store`, one cycle | beta |
|---|---|---|
| `two-way-conflict` | `synchronized: 0 change(s) to alpha, 22 change(s) to beta` | **0 files** (was 20) |
| `two-way-paranoid` | `synchronized: 0 change(s) to alpha, 22 change(s) to beta` | **0 files** (was 20) |

No halt, no conflict, no blocked path — the cycle reports success while deleting every file on the side that still had them. The count of 22 is the 20 files plus the directory entries.

This is C-1, reproduced on macOS from the outside. The mechanism was confirmed by reading: `one_side_emptied_root` (`src/session/mod.rs:1335`) decides a side is gone by `children().is_empty()`, and `src/scan/mod.rs:730` records an ignored entry as an `Untracked` child — so the shipped default ignore of `.DS_Store` does not save it; it is what defeats the guard. Any leftover entry does the same.

### 7b. A directory swapped by rename, under FSEvents — F-H4

1. Sync `a/live/f1` and `a/staging/f1` (different contents) to a beta, under `watch`.
2. On the alpha: `mv a/live a/old && mv a/staging a/live`.

**Expected:** within one cycle, the beta has the new `live/f1` and `old/f1`, and no `staging`. **Before the fix, on Linux:** the old `live` content stayed for up to two minutes. Record whether FSEvents' per-directory events already hide this on macOS.

**Record:** the beta's tree after one cycle, and after 30 seconds.

**Result — `d7c2e21` plus fny's uncommitted peering rename, macOS 26.5.1, Apple M4, 2026-09-24. Reproduced; FSEvents does not hide it, and it did not heal at the two-minute walk.**

`a/live/f1` = `live-one`, `a/staging/f1` = `staging-one`, synced, then `mv a/live a/old && mv a/staging a/live`.

| when | beta `live/f1` | beta `old/f1` | shape |
|---|---|---|---|
| 8 s | `live-one` (stale) | `live-one` | `old/` and `live/`, no `staging/` ✓ |
| 30 s | `live-one` (stale) | `live-one` | unchanged |
| 2 m 30 s | `live-one` (stale) | `live-one` | unchanged |
| after `flush` | `live-one` (stale) | `live-one` | unchanged |
| after `verify` | `staging-one` ✓ | `live-one` | correct at last |

The directory *names* move within a cycle, so the tree shape looks right immediately — which is what makes it dangerous. The content does not: beta ends up holding two copies of the old file and no copy of the new one, while the session reports `synchronized`. Alpha's `live/f1` said `staging-one` throughout.

Unlike the Linux note, the two-minute full walk did not fix it here, and neither did a forced `flush`. Only `verify`, which re-reads every byte, healed it — so on macOS the stale content survives until something forces a content re-read, not merely a rescan.

**Filed against** H-4, which now carries this reproduction.

### 7c. A deep tree — F-H5

macOS limits a path to 1,024 bytes, not Linux's 4,096, so a `d/d/d/…` chain stops at about 510 levels by path length.

1. Build the deepest chain the shell allows: `for i in $(seq 600); do mkdir d; cd d; done`, noting where it fails.
2. Run `autobahn sync` on it to a local beta.

**Expected:** no abort. Either it syncs, or the too-deep part is reported as a problem.

**Record:** the depth reached, and the outcome. This tells us whether the stack overflow can happen on macOS at all.

**Result — `d7c2e21` plus fny's uncommitted peering rename, macOS 26.5.1, Apple M4, 2026-09-24. Passes.**

The shell reached the full 600 levels (each `mkdir d; cd d` keeps the *relative* path short, so the chain is only bounded when something walks it absolutely). `autobahn sync` did not abort and did not overflow. It synced 500 levels to the beta and reported the rest as a problem on alpha:

```
alpha problem at "d/d/d/…/d": unable to probe entry: File name too long (os error 63)
```

So macOS's 1,024-byte path limit is reached at depth ~500 — `d/` is two bytes — and it arrives as a blocked path, which is the "reported as a problem" half of the expectation. No stack overflow is reachable this way on macOS.

**Filed against** H-5, which now records that macOS's path limit prevents it.

### 7d. The example alert hook with a hostile summary — F-H27

1. Point `on_alert` at the example `~/.autobahn/on-alert.sh`, with `terminal-notifier` not installed.
2. In the beta, create a file whose name makes a blocked-path alert. Use a directory without read permission, containing `x" & (do shell script "touch /tmp/mb7-pwned") & "`.
3. Wait for the alert. If the summary turns out not to include the file name, record that. The same test then needs a halted session whose error text carries the name, and GLM's report names that as the other route.

**Expected after the fix:** a notification showing the name as plain text, and no `/tmp/mb7-pwned`. **Before the fix:** `/tmp/mb7-pwned` appears. That is a harmless `touch`, but delete it after.

Also check that a pre-fix `on-alert.sh` is rewritten at startup, and that an edited one is left alone with a warning.

**Record:** the notification text, and whether the file appeared.

**Result — `d7c2e21` plus fny's uncommitted peering rename, macOS 26.5.1, Apple M4, 2026-09-24. Reproduced: `/tmp/mb7-pwned` appeared.**

The hook was the one `autobahn init` writes, with only the `terminal-notifier` branch deleted, to stand in for "not installed". It was then run the way `src/alerts.rs:488` runs it — `sh -c <hook>`, values passed by environment — with

```
AUTOBAHN_SUMMARY=voltai → fny: halted: x" & (do shell script "touch /tmp/mb7-pwned") & "
```

The hook exited 0, printed nothing, and `/tmp/mb7-pwned` existed two seconds later. The injectable line is `src/config.rs:164`:

```sh
exec /usr/bin/osascript -e \
    "display notification \"$AUTOBAHN_SUMMARY\" with title \"autobahn\""
```

(the file has been removed again).

**On the route, which changes the severity rather than the bug.** A blocked path does *not* put a name in the summary: `alert_summary` (`src/supervisor/mod.rs:1973-1979`) emits only `plural(n, "blocked path")`. Names reach the summary through the `halted` and `errored` arms (`:1966-1971`), which append the last clause of `status.error` — and that clause carries paths and peer-supplied text. So the filename route the section describes needs an error, not a blocked path.

**Filed against** H-27, which already describes this; this is its first execution on a Mac.

### 7e. A replaced root under FSEvents — F-M-OBS (M-27)

1. `watch` a pair.
2. `mv alpha alpha.old && cp -a alpha.old alpha`.
3. Edit a file in the new `alpha`.

**Expected:** the edit reaches the beta within a cycle, not after the two-minute walk. Also check that edits in `alpha.old` are ignored.

**Record:** the delay, for both.

**Result — `d7c2e21` plus fny's uncommitted peering rename, macOS 26.5.1, Apple M4, 2026-09-24. Passes, both halves.**

`mv alpha alpha.old && cp -a alpha.old alpha`, then an edit at 02:28:44. The cycle for it is logged at 02:28:44, and the beta held the new content within ten seconds. An edit made afterwards in `alpha.old` never reached the beta.

### 7f. Chmod through a hard link — LOCAL-11

macOS has no `fs.protected_hardlinks`.

1. Create `~/mb7/outside/tool.sh`, mode `0644`, and hard link it into the alpha as `alpha/tool.sh`.
2. On the beta, make `tool.sh` executable.
3. Run a cycle.

**Expected after the fix:** `alpha/tool.sh` becomes executable, and `~/mb7/outside/tool.sh` stays `0644`, because the link was broken. **Before the fix:** both become executable.

**Record:** both modes, and `ls -li` to show whether the link still exists.

**Result — `d7c2e21` plus fny's uncommitted peering rename, macOS 26.5.1, Apple M4, 2026-09-24. Reproduced, and it rewrites more than the execute bit.**

`~/mb7/outside/tool.sh` at `0644`, hard-linked into the alpha; the beta's copy made executable; one cycle:

```
before  139616704 -rw-r--r--  2  a/tool.sh          (same inode, 2 links)
        139616704 -rw-r--r--  2  outside/tool.sh
after   139616704 -rwx------  2  a/tool.sh
        139616704 -rwx------  2  outside/tool.sh
```

The link is intact (still one inode, two links), so the chmod went straight through to the file outside the synchronization root — LOCAL-11 as written. Worth adding to that ticket: the outside file did not merely gain `+x`, it was rewritten to the session's configured `file_mode` (`0700`), so it also *lost* the group and world read bits it had.

**Filed against** [`LOCAL-11`](REVIEWS/fixes/LOCAL-11-hardlink-chmod.md) and L-3.

### 7g. Running under `sudo` — LOCAL-08

`sudo` on macOS keeps the caller's `$HOME`.

1. `sudo autobahn watch --state-root ~/mb7/state`, with any config.

**Expected after the fix:** refused, with a message naming the root and home mismatch, and `~/mb7/state` still owned by you. **Before the fix:** root-owned files appear under it. Check with `ls -la`, and clean up with `sudo rm -rf ~/mb7/state`.

**Record:** the message, and the ownership.

**Result — `d7c2e21` plus fny's uncommitted peering rename, macOS 26.5.1, Apple M4, 2026-09-24. Reproduced, with two consequences the check did not anticipate.**

```
whoami: root
HOME:   /Users/faraz
supervising 10 session(s); status is available via `autobahn status`
2026-09-24 07:55:53 peering: leading as the alpha at term 1
```

- **Root-owned state, as expected:** `control.sock`, `sessions/`, `status/` and `supervisor/` under `~/mb7/state`, a directory owned by the user. No message, no refusal.
- **`--state-root` moved the state but not the configuration.** `$HOME` stays the caller's under macOS `sudo`, so the config resolved to `~/.autobahn/config.toml` and root supervised **the live fleet** — ten sessions against real roots and hosts — not the throwaway pair. The endpoint locks refused each one, which is the only thing that stopped two supervisors writing the same trees.
- **It reached the live state root.** `~/.autobahn/peering/lease.json` is now owned by root and the user's supervisor cannot renew it. Repair needs `sudo chown`.

**Is it macOS-specific?** The default path is; the gap is not. Measured on both: macOS `sudo` keeps `HOME=/Users/faraz`, while Ubuntu's `Defaults env_reset` gives `HOME=/root`, so on Linux the same command reads root's own config and writes root's own state. `sudo -E` puts Linux in the same position.

**Filed against** [`LOCAL-08`](REVIEWS/fixes/LOCAL-08-refuse-root.md), which now carries this run and a platform-neutral clause: refuse when the config or state root is owned by another user, whatever the uid.

### 7h. The control socket with a long state root — LOCAL-05

1. Use a state root deep enough that its path passes 100 bytes, and start `watch`.
2. Find the socket: `lsof -U | grep autobahn`.

**Expected:** the socket is under macOS's per-user `$TMPDIR` or another private directory, owned by you, with the directory at `0700`. `autobahn status` works.

**Record:** the socket path and its directory's mode.

**Result — `d7c2e21` plus fny's uncommitted peering rename, macOS 26.5.1, Apple M4, 2026-09-24. Passes on macOS.**

A 122-byte state root. The socket is not in it; it fell back to

```
/var/folders/lx/…/T/autobahn-501/d1a312d66e32819e.sock
drwx------@  /var/folders/lx/…/T/autobahn-501
```

which is macOS's per-user `$TMPDIR`, owned by me, mode `0700`. `autobahn status` worked against it.

Evidence for LOCAL-05: on macOS the fallback is already private, because `$TMPDIR` is per-user here. The hazard that ticket describes — another user creating the directory first — needs a shared `/tmp`, which is the Linux case.

**Filed against** [`LOCAL-05`](REVIEWS/fixes/LOCAL-05-control-socket-fallback.md): no change needed for macOS.

### 7i. The menu bar app — CI-09, LOCAL-04, F-H30

1. `cargo test --release --features tray` passes locally. This is what CI-09 adds to CI.
2. **Show diff** on a conflict writes its file under `~/.autobahn/tmp/`, not the shared temp directory, and the file opens.
3. Create conflicts on two top-level files, one named `--all`. Settle `--all` from the menu. **Expected:** only that conflict is settled. **Before the fix:** both are.

**Record:** the test output, the diff file's path, and which conflicts were settled.

**Result — `d7c2e21` plus fny's uncommitted peering rename, macOS 26.5.1, Apple M4, 2026-09-24. Step 1 fails, for a platform-dependent assertion.**

```
cargo test --release --features tray
test result: FAILED. 386 passed; 1 failed
  transport::install::tests::the_remote_command_runs_under_sh_and_picks_this_platform
  assertion `left == right` failed: left: Some(126), right: Some(127)
```

It fails identically without `--features tray`, so the tray feature is not the cause: the suite simply does not pass on macOS. The assertion at `src/transport/install.rs:767` expects a missing agent to make `sh` exit 127. Measured directly on both platforms:

| | missing file under `exec` | unexecutable file |
|---|---|---|
| macOS `sh` | **126** | 126 |
| Linux `sh` | 127 | 126 |

So the launcher behaves correctly on both; only the test's expectation is Linux-only. This matters for CI-09, which proposes adding these tests to CI — a macOS runner would go red on arrival. Filed as [`MAC-5`](REVIEWS/fixes/MAC-5-sh-exit-code-assertion.md).

Two more tests fail on macOS for the same kind of reason, found while running the suite in full: `a_wedged_supervisor_is_unresponsive_within_the_client_timeout` and `a_status_report_against_a_wedged_supervisor_returns` both panic with *the backlog never filled* (`src/supervisor/control.rs:1586`). The fixture wedges a supervisor with `listen(fd, 0)`; macOS keeps its own minimum backlog, so the queue never fills. Both reproduce on untouched `main`. Filed as [`MAC-6`](REVIEWS/fixes/MAC-6-control-socket-backlog-test.md) (L-44). With MAC-5 fixed, the suite on macOS is **604 passed, 2 failed**, and those two are MAC-6.

Steps 2 and 3 (the diff scratch path, and `--all` as a filename) still need the menu, and a person to click it.

### 7j. Escape sequences in file names — F-M-OUT

1. In `bash`, create a conflict on a file whose name carries an OSC 52 clipboard write: `printf -v n 'x\033]52;c;%s\a' "$(printf pwned | base64)"; touch "$n"`, on both sides with different contents.
2. Copy some known text to the clipboard.
3. Run `autobahn status`, `autobahn issues`, and open the shop, in Terminal.app and in iTerm2. For iTerm2, enable "Applications in terminal may access clipboard" for the test.

**Expected after the fix:** the name shows with a visible `\x1b`, and the clipboard is unchanged. **Before the fix:** the clipboard holds `pwned`, in terminals that honour OSC 52.

**Record:** each terminal's result.

**Result — `d7c2e21` plus fny's uncommitted peering rename, macOS 26.5.1, Apple M4, 2026-09-24. The raw escape is emitted; the clipboard half still needs a person.**

Two files named `x\x1b]52;c;cHduZWQ=\x07` with different contents, one conflict. `autobahn issues`, piped through `cat -v`:

```
    1 conflict
      x^[]52;c;cHduZWQ=^G
        alpha  10 B, modified 14s ago
        /Users/faraz/mb7/j8/b 9 B, modified 14s ago
      fix: autobahn resolve esc x^[]52;c;cHduZWQ=^G --keep alpha|…|both
```

`^[` is ESC and `^G` is BEL, so the OSC 52 sequence reaches the terminal intact — and a second time inside a `resolve` command the reader is invited to copy. In a terminal that honours OSC 52 (iTerm2 with the setting enabled) that writes the clipboard.

Not everything is raw: `sync`'s own conflict line printed the name escaped, as `"x\u{1b}]52;c;cHduZWQ=\u{7}"`. So the sanitising exists in one path and not the other, which is M-6's point exactly. Whether each terminal acts on it was not tested — that needs a person at Terminal.app and iTerm2.

**Filed as** [`F-M6`](REVIEWS/fixes/F-M6-terminal-escapes-in-issues.md) (M-6).

### 7k. The app build and signing — HYG-1, CI-05

1. `apps/macos/build.sh`, then `plutil -p apps/macos/Autobahn.app/Contents/Info.plist | grep -i version`. **Expected:** it matches `Cargo.toml`'s version.
2. After CI-05 splits building from signing, `apps/macos/release.sh` with no arguments still builds, signs, notarises and staples in one go on a laptop.

**Record:** the plist versions, and whether `spctl -a -vv` accepts the app.

**Result — `d7c2e21` plus fny's uncommitted peering rename, macOS 26.5.1, Apple M4, 2026-09-24. Step 1 passes.**

```
CFBundleShortVersionString => 0.4.0
CFBundleVersion            => 0.4.0
LSMinimumSystemVersion     => 11.0
```

Both match `Cargo.toml`'s `version = "0.4.0"`. `spctl -a -vv` says `rejected — source=Unnotarized Developer ID`, which is correct for a locally built app: `build.sh` signs with the Developer ID but does not notarise. Step 2 waits for CI-05.

### 7l. The Intel build under Rosetta — optional, from the wishlist

1. `rustup target add x86_64-apple-darwin && cargo test --release --target x86_64-apple-darwin`. It runs under Rosetta 2.
2. `arch -x86_64 target/x86_64-apple-darwin/release/autobahn sync` on a small pair.

**Record:** pass or fail, and the time against the native run. This is the cheapest evidence for or against keeping the Intel build published.


## Notes from the run of 2026-09-24

**The login service could not be moved onto this build.** `autobahn restart` refused, correctly: *group 'shared': mode 'peering-alpha-experimental' was renamed to 'peering-alpha-dangerously-experimental'*. The live `~/.autobahn/config.toml` still uses the old spelling, and until it is changed the service keeps running its 2026-09-22 binary. That is why sections 1 and 5 were done in throwaway state roots under `watch`, and why the *another build* check and the app's own lines are still open.

**Everything above ran against a clean build of `d7c2e21`** (`cargo build --release`, plus `apps/macos/build.sh` for the app), on macOS 26.5.1, Apple M4, on AC power.
