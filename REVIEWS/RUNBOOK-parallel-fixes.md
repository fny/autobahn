# Runbook: implementing the review tickets in parallel on a dedicated EC2 instance

Written 2026-09-24. **Executed 2026-09-24**; see `IMPLEMENTATION.md` for the outcome. The instance is stopped. The goal is to implement the v1 tickets in `REVIEWS/fixes/` with several Claude Code agents working in parallel, without colliding with each other, this machine's live state, or GitHub.

## 0. Principles

1. **Nothing sensitive goes to GitHub.** `fny/autobahn` is public. `REVIEWS/` describes unfixed vulnerabilities in detail, including exploit shapes. The work therefore moves by `rsync` over SSH, never by `git push`. `REVIEWS/` is never committed. Fix commits reach GitHub only when you ask, and only once the fixes are in.
2. **One source of truth while this runs.** From the copy in §3 until the results come back in §8, the EC2 instance holds the working copy. Nobody edits autobahn on this machine in the meantime, and the other autobahn session here (`eb31e2e4`, "remote") is stopped first.
3. **Lanes own files.** Tickets that touch the same hot file run in one lane, in order. Lanes run in parallel.
4. **One integrator lands everything.** Agents never write to the integration branch. The integrator replays lane commits in order and runs the full suite after each lane. When there is a merge conflict, it stops and asks you, per your global rule, rather than resolving it.
5. **Every agent has its own everything:** git worktree, build directory, `HOME` for tests, and `AUTOBAHN_HOME`.
6. **Parallelize as much as possible.** Split the work into as many agents as file ownership allows, at every stage: wave 0, wave 1, wave 2, and inside each lane. The only reason to run two tickets one after the other is that they edit the same code. Inside a lane, an agent uses sub-agents for anything that doesn't edit files: reading code, checking a ticket's premise, running test suites, and reviewing its own diff.
7. **The instance runs until ALL the work is done.** It is not stopped between waves or overnight. It is stopped (not terminated) only after every wave is integrated, the results are home and verified here, and `REVIEWS/` is copied back. Terminating it is a later, separate decision (§10).

## 1. Before provisioning (on this machine)

- [ ] **Stop what could interfere:**
  - interrupt or end the stalled `eb31e2e4` session (`joy abort eb31e2e4` or `joy kill eb31e2e4`);
  - kill the hung test run: `kill 1456007 1454093`, which is `cargo test --test supervisor an_edited_configuration`, stuck for about 10 hours;
  - leave the AWS bench jobs (`bench-1790219288`) to finish on their own.
- [ ] **Snapshot the working tree** as a rollback point that doesn't depend on git:
  ```sh
  cd ~/Workspace && tar --exclude='autobahn/target' -czf ~/autobahn-snapshot-$(date +%F).tgz autobahn
  ```
- [ ] **Record the starting point:**
  ```sh
  cd ~/Workspace/autobahn
  git rev-parse HEAD > ~/autobahn-base-commit
  git status --porcelain > ~/autobahn-base-status
  git diff | sha256sum > ~/autobahn-base-diff.sha256
  ```
  Also check the git index. `.git` is synced from the Mac and its index goes stale here, so run `git reset -q` before trusting `git status`.
- [ ] **Decide who holds the Claude credentials.** Per your rules, the agents need `ANTHROPIC_API_KEY` set from `CLAUDE_API_KEY`. Fetch it from Doppler on the instance, or paste it in. Either way it lives only in the instance's environment.

## 2. Provision the instance

Profile `fde`, region `us-east-2`, the same account the bench fleet uses. Suggested size: `c7i.16xlarge` (64 vCPU, 128 GB), with a 600 GB gp3 volume. Wave 1 runs about 16 lane agents at once, each with its own build directory, plus the integrator. That is about $2.90 an hour on demand; check current pricing before launching.

A shared `sccache` cache keeps 16 parallel builds of the same dependencies from each compiling everything from scratch. §3 sets it up.

```sh
P="--profile fde --region us-east-2"
RUN=autobahn-fixes-$(date +%Y%m%d)
aws $P ec2 create-key-pair --key-name $RUN-key --query KeyMaterial --output text > ~/.ssh/$RUN-key.pem
chmod 600 ~/.ssh/$RUN-key.pem
VPC=$(aws $P ec2 describe-vpcs --filters Name=is-default,Values=true --query 'Vpcs[0].VpcId' --output text)
SG=$(aws $P ec2 create-security-group --group-name $RUN-sg --description "$RUN" --vpc-id $VPC --query GroupId --output text)
aws $P ec2 authorize-security-group-ingress --group-id $SG --protocol tcp --port 22 \
    --cidr $(curl -s https://checkip.amazonaws.com)/32
AMI=$(aws $P ssm get-parameter --name /aws/service/canonical/ubuntu/server/24.04/stable/current/amd64/hvm/ebs-gp3/ami-id \
    --query Parameter.Value --output text)
ID=$(aws $P ec2 run-instances --image-id $AMI --instance-type c7i.16xlarge --key-name $RUN-key \
    --security-group-ids $SG --instance-initiated-shutdown-behavior stop \
    --block-device-mappings '[{"DeviceName":"/dev/sda1","Ebs":{"VolumeSize":600,"VolumeType":"gp3","Iops":6000,"Throughput":500,"DeleteOnTermination":true}}]' \
    --tag-specifications "ResourceType=instance,Tags=[{Key=Name,Value=$RUN}]" \
    --query 'Instances[0].InstanceId' --output text)
aws $P ec2 wait instance-running --instance-ids $ID
HOST=$(aws $P ec2 describe-instances --instance-ids $ID --query 'Reservations[0].Instances[0].PublicIpAddress' --output text)
echo "$RUN $ID $HOST" | tee ~/autobahn-fixes-instance
```

Shutdown behaviour is `stop`, not `terminate`, so an accidental `shutdown` keeps the disk. Add `Host autobahn-fixes` to `~/.ssh/config` with that key and host.

## 3. Bootstrap the instance

```sh
ssh autobahn-fixes
sudo apt-get update && sudo apt-get install -y build-essential pkg-config git tmux jq shellcheck \
    python3 python3-venv openjdk-17-jre-headless rsync
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal -c clippy,rustfmt
cargo install sccache --locked && echo 'export RUSTC_WRAPPER=sccache SCCACHE_DIR=$HOME/.sccache SCCACHE_CACHE_SIZE=60G' >> ~/.bashrc
curl -fsSL https://fnm.vercel.app/install | bash && fnm install 24 && npm i -g pnpm @anthropic-ai/claude-code
pnpm add -g @fny/joy-daemon && joy auth <relay url> && joy start   # the URL `joy auth` prints on this machine; you approve the pairing in the app
```

- **Checks:** `joy doctor` is clean, the instance appears among your machines in the app, and `claude --version` works.
- **Credentials:** set `ANTHROPIC_API_KEY` from `CLAUDE_API_KEY` in the environment joy passes to its sessions, with `joy env set` on *that* machine only.

## 4. Copy the working tree and establish a baseline

From this machine:
```sh
rsync -a --delete --exclude target/ --exclude 'bench/harness/target/' \
    ~/Workspace/autobahn/ autobahn-fixes:~/autobahn/
```

On the instance:
```sh
cd ~/autobahn && git reset -q && git status --porcelain > /tmp/status && git diff | sha256sum
```
Compare with `~/autobahn-base-status` and `~/autobahn-base-diff.sha256` from §1. They must match.

- **Make a base commit, locally only.** On the instance, create a local branch `integration` and commit the whole current working tree to it, *excluding* `REVIEWS/`, as "wip: state before the review fixes". That commit exists only on the instance. It gives every lane one common starting point. Add `REVIEWS/` to `.git/info/exclude`, so it can never be committed there.
- **Record a baseline.** Run the full suite once, with isolation (§5), and record the result in `~/baseline.txt`: `cargo fmt --check`, `cargo clippy --release --all-targets -- -D warnings` (expect the 3 CI-01 errors), and `cargo test --release`. Known failures on the baseline aren't regressions later.

## 5. Isolation for each lane

Each lane `L` gets a worktree, which is safe here because this `.git` isn't synced anywhere:

```sh
cd ~/autobahn && git worktree add ~/lanes/$L -b lane-$L integration
mkdir -p ~/lanes/$L.home ~/lanes/$L.tmp
cat > ~/lanes/$L.env <<EOF
export CARGO_TARGET_DIR=$HOME/lanes/$L.target
export AUTOBAHN_HOME=$HOME/lanes/$L.home/.autobahn
export TMPDIR=$HOME/lanes/$L.tmp
export CARGO_PROFILE_RELEASE_LTO=false      # fast iteration; the integrator builds with LTO
export CARGO_BUILD_JOBS=4                   # 16 lanes × 4 jobs on 64 vCPUs
EOF
```

- **Tests see a private home.** Run tests as `HOME=$HOME/lanes/$L.home cargo test …`. `CARGO_HOME` and `RUSTUP_HOME` stay pointing at the real toolchain. A per-lane `HOME` for tests matters until F-M-TEST's isolation lands, because some unit tests still write to `$HOME/.autobahn`. Each lane then writes only to its own.
- **Branches stay on the instance.** Lane branches are local to it and never pushed. Your no-branches rule is about `main` in the synced repo, and nothing here touches that.

## 6. The work, in waves, parallel wherever files allow

Only tickets marked for v1, or unmarked and proposed, are in scope. **Out:** every PEER ticket, LOCAL-10, the wishlist, `TODO-SPEED.md`, and REL-1 step 2 if you decide it can wait.

**The rule for splitting.** Two tickets go in separate lanes unless they edit the same function or the same small file. Two lanes may share a large file, such as `src/endpoint/local.rs` or `src/main.rs`, when they own different, named regions of it. Git merges non-overlapping hunks cleanly. The integrator catches the rest.

### Wave 0: foundations, 4 agents in parallel

| Lane | Work | Files |
|---|---|---|
| 0a | CI-01, the clippy fix | `src/endpoint/local.rs` (`Receiving`), `src/endpoint/remote.rs` (two type aliases) |
| 0b | F-M-TEST's M-53 part (test isolation, unique session ids, no global `set_var`), then T1-2's validator and its `create_endpoint` call | `src/transport/mod.rs` (`create_endpoint`), `src/transport/mux.rs` tests, `src/protocol.rs`, `tests/supervisor.rs`, `src/transport/install.rs` tests |
| 0c | LOCAL-01, `src/fsutil.rs` | new file |
| 0d | `shell_quote` (F-H29) and `display_safe` (F-M-OUT) | new `src/text.rs` |

The integrator lands all four and runs the full suite, then tags `wave-0`. Every wave 1 lane branches from `wave-0`.

### Wave 1: about 16 agents in parallel

| Lane | Area, and the region it owns | Tickets, in order |
|---|---|---|
| A1 | Supply and staging receive: `local.rs` `supply_*`, `stage_begin`, `open_receive_file`, `base_signature` | T1-1, T1-5, F-H7 |
| A2 | The apply path: `local.rs` `rename`, `read_file`, `validate_name`, `remove_directory`, publishing, staging root | T1-3 with the L-20 fix, T1-4 with LOCAL-03, F-H8, F-M-STAGE, LOCAL-11 |
| B1 | Observer: `src/endpoint/observer.rs`, and `local.rs` `transition`'s generation handling | F-H3, F-M-OBS |
| B2 | Scanner: `src/scan/mod.rs`, plus thread spawning for stack sizes | F-H4, F-H5, LOCAL-09 |
| C1 | Reconcile and the emptied-root halt: `src/tree/reconcile.rs`, `src/session/mod.rs` (the halt), `src/tree/mod.rs` | F-C1, F-M-STATE's M-33 part |
| C2 | Journal: `src/session/ancestor.rs`, and `src/endpoint/mod.rs` (`achieved_changes`) | F-M-STATE's M-32 part, F-L-STATE |
| D1 | Resolve, and output to the terminal: `main.rs` `run_resolve`, `blocked_fix`, status and issues printing; `src/shop.rs`; `src/pager.rs`; `src/tray.rs` commands | F-H1 stage 1, F-H29, F-H30, F-M-OUT's terminal part, HYG-2 |
| D2 | Config topology and identity: `src/config.rs`, plus the `run_sync` topology call | F-H2 with F-H25, F-H12, OPS-6 |
| D3 | The rest of the CLI: `main.rs` `run_sync` (non-topology), `run_clean`, `select`, `sync` exit codes | OPS-1, OPS-2, the rest of F-L-MISC |
| E1 | Supervisor: `src/supervisor/mod.rs`, `src/supervisor/reload.rs`, `src/supervisor/control.rs` (server side), `src/logging.rs` | F-H13 with OPS-7, F-M-SUP's M-34 and M-38 parts |
| E2 | Transport: `src/transport/mux.rs`, `src/transport/install.rs`, `src/transport/mod.rs` (non-`create_endpoint`), `src/endpoint/remote.rs` | F-M-SUP's M-12 part A, OPS-3, OPS-4, T2-1, F-M-OUT's prune and stderr parts |
| E3 | Alerts: `src/alerts.rs`, and the `config.rs` example hook | F-H27, F-M-SUP's M-35 part |
| F1 | Updater and service: `src/update.rs`, `src/service.rs`, `scripts/install.sh` | REL-1 step 1, LOCAL-06, F-M-UPD, LOCAL-08 |
| F2 | Local permissions: `src/persist.rs`, `src/paths.rs`, the `diff` scratch, the control socket fallback | LOCAL-02, LOCAL-04, LOCAL-05 |
| F3 | CI and spec: `.github/`, `spec/check.sh` | CI-02, CI-03, CI-04, CI-06, CI-07, CI-08, CI-09, CI-10 |
| F4 | Bench tooling: `bench/`, `scripts/mi` | BENCH-1 to BENCH-6, LOCAL-07 |
| F5 | The macOS build scripts: `apps/macos/` | HYG-1, CI-05 |

Known shared spots:
- **`src/main.rs`** is split between D1, D2 and D3 by function. E1 and F2 may add small calls there.
- **`src/endpoint/local.rs`** is split between 0a, A1, A2 and B1 by function.
- **`src/transport/mod.rs`:** 0b owns `create_endpoint`. E2 owns everything else in it.
- **`src/config.rs`:** D2 owns it. E3 touches only the example-hook string.

A lane that finds it must edit outside its region stops and reports to the integrator rather than editing.

### Wave 2: 3 agents in parallel, after wave 1 is integrated

| Lane | Work |
|---|---|
| 2a | **The compatibility-epoch bump,** as one commit: F-M-SUP part B (progress frames and silence detection), the remote half of P-13 (`one_shot` in `Initialize`), removing `Response::Scan` together with the rest of HYG-4, and checksummed journal headers if C2 left them for the bump. |
| 2b | **Docs describing the final code:** HYG-3, OPS-5, the T1 confinement rule, and the I9 wording in INVARIANTS and `safety.md`. Rebase once onto 2a at the end. |
| 2c | **The rest of F-M-TEST:** the standing-watch and cut-oracle fixes, and marking the TLC tests ignored. Also the 200-run loop of the arm64 fan-out test, on a short-lived arm64 instance this lane launches and terminates itself. |

## 7. What every lane agent is told

Start each lane session in its worktree, with its env file sourced, and give it this brief, filling in the lane letter and ticket list:

> You are lane <L>. Implement these tickets, in this order: <list>. Each ticket file in `REVIEWS/fixes/` holds the confirmed problem, the agreed fix and the required tests. The copy of `REVIEWS/` on this machine is at `~/autobahn/REVIEWS`, and it is read-only for you.
>
> Rules:
> - Touch only files your tickets need. If a ticket needs a change in a file another lane owns (see the lane table in `REVIEWS/RUNBOOK-parallel-fixes.md`), make the smallest change and say so in the commit message.
> - For each ticket:
>   1. write its tests first and watch them fail;
>   2. implement the fix;
>   3. run `cargo fmt`, `cargo clippy --release --all-targets -- -D warnings`, and the affected tests, with `HOME=$HOME/lanes/<L>.home`;
>   4. commit once, with the message style the repository uses (see `git log`), naming the ticket ID in the body.
> - Never edit `REVIEWS/`, never push, never rebase onto another lane, and never resolve a conflict with another lane's work. Report it instead.
> - Parallelize inside your lane. Use sub-agents for everything that doesn't edit files: reading code, checking each ticket's premise before you start it, running the test suites, and reviewing your diff before committing. Edits stay yours, one ticket at a time.
> - A ticket whose premise turns out wrong on inspection: stop that ticket, write a short note to `~/lanes/<L>.notes.md` explaining why, and move on.
> - When done, write `~/lanes/<L>.done.md`: one line per ticket (done, skipped, or blocked, with a reason) and the final test output summary.

Sessions are started with `joy new ~/lanes/<L> --agent claude -m "<brief>"` on the instance, so all of them appear in your app.

## 8. Integration and bringing the results home

**The integrator** is a Claude session in `~/autobahn` on the `integration` branch. It:

1. **Lands each lane as soon as it finishes,** rather than waiting for the whole wave. It watches for `~/lanes/<L>.done.md`, or uses `joy wait`.
2. **Replays each lane onto `integration`** with `git cherry-pick`. Every half hour it also trial-merges all unfinished lanes in a scratch worktree, so a conflict shows up while that lane can still adjust.
   - When several lanes finish at once, land pure logic before the code that calls it: C1 and C2, then B, A, E, and the `main.rs` lanes last.
3. **After each lane, runs the checks in parallel,** through sub-agents or separate build directories:
   - `cargo fmt --check`;
   - clippy with LTO on;
   - the full `cargo test --release`, with isolation;
   - `spec/check.sh quick` plus the TLC replay, once CI-02 and CI-03 have landed.
4. **On a conflict or a new failure, stops and messages you.** You get a summary, then each conflict one at a time with a proposed resolution, per your global rule.
5. **Tags each wave:** `wave-1`, `wave-2`.

The integrator also keeps `REVIEWS/FINAL.md` current, adding a dated "Implemented in `<commit>`" line to each finished ticket's entry.

**Bringing it home, after each wave or at the end:**

- **Back to this machine as patches, not a fetch.** Pull them into the synced repo with `git format-patch base..integration`, and apply them on `main` there, committing with pathspecs as usual.
  - **Why not a fetch:** it would write the lane branches into the `.git` your Mac syncs.
  - **Why no stale-index risk:** `git am` builds each commit from its patch, not from the index. Still, run `git reset -q` first, since this is the repo whose index goes stale.
- **The WIP base commit.** The patch series starts with the "wip: state before the review fixes" commit. That commit represents your uncommitted changes, which are already in the working tree here, so leave it out and apply from the commit after it.
- **Copy `REVIEWS/` back** with `rsync`, so the implemented notes and lane notes come home too.
- **GitHub.** Pushing is a separate decision you make at the end: fix commits only, never `REVIEWS/`.

## 9. Watching it and controlling cost

- **Where to watch:** the joy app shows every lane session, its state, and when one needs input.
- **No stopping mid-run.** The instance runs continuously until ALL the work is done: every wave integrated, the results applied and verified on this machine, and `REVIEWS/` copied back. Stopping it earlier would kill every running lane session.
- **A rough budget:** with this much parallelism, wave 0 takes a few hours, wave 1 roughly a day of wall-clock time, and wave 2 a few hours. The EC2 cost is about $2.90 an hour, running continuously. Most of the cost is agent tokens, not EC2.

## 10. When ALL the work is done: stop the instance

Stop, don't terminate, and only once *all* of these are true:
- every wave is integrated;
- the patches are applied on this machine;
- the full suite is green here;
- `REVIEWS/` is copied back;
- a final archive of `~/autobahn` and `~/lanes/*.md` is copied home.

```sh
read RUN ID HOST < ~/autobahn-fixes-instance
aws --profile fde --region us-east-2 ec2 stop-instances --instance-ids $ID
```

A stopped instance keeps its disk, about $50 a month for 600 GB, so it can be restarted for follow-ups.

**Terminating it is a separate, later decision.** When you're sure it's no longer needed:

```sh
read RUN ID HOST < ~/autobahn-fixes-instance
P="--profile fde --region us-east-2"
aws $P ec2 terminate-instances --instance-ids $ID && aws $P ec2 wait instance-terminated --instance-ids $ID
aws $P ec2 delete-security-group --group-name $RUN-sg
aws $P ec2 delete-key-pair --key-name $RUN-key && rm ~/.ssh/$RUN-key.pem
joy machines   # remove the instance from the relay in the app
```

The final archive from the checklist above should already be home before terminating.

## 11. Risks

| Risk | Mitigation |
|---|---|
| `REVIEWS/` leaks to GitHub | Copied by rsync only, listed in `.git/info/exclude` on the instance, and never pushed. |
| Lanes conflict | Lanes own their files, lanes land in a fixed order, and the integrator stops on a conflict instead of guessing. |
| Tests touch real state | A per-lane `HOME` and `AUTOBAHN_HOME`, and wave 0 fixes the tests themselves. |
| Two wire changes race | One agent owns the epoch bump, in wave 2. |
| The instance dies | Commits live on its disk, stopping keeps the disk, the snapshot from §1 is the fallback, and patches come home after each wave. |
| A ticket's premise is wrong | The agent stops that ticket and writes a note. The integrator raises it with you. |
| Someone edits autobahn elsewhere meanwhile | Principle 2. The last-edit times of `~/Workspace/autobahn` are checked before applying patches in §8. |
