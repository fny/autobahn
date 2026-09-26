# F-M-UPD: An update installs what the service runs, and can roll all of it back

**Findings:**
- M-40: rollback restores only the binary, not the agent bundle (ASTRA F24; OPUS Low).
- M-41: a successful update doesn't prove the service runs the new binary (ASTRA F25).
- M-21: service arguments aren't safely written (ASTRA F26; DEEPSEEK F11; KIMI ABN-L2; OPUS M15, M16).

**Status:** proposed. Medium. Land it with REL-1 and LOCAL-06, which change the same updater.

## Problems

- **M-40: a half rollback.** `update::run` (`src/update.rs:74-164`) replaces the agent bundle in step 4, before the binary and the restart, and `refresh_agents` deletes the old bundle as soon as the swap succeeds (`:377-396`). If the restart then fails, `restart_service` (`:215-270`) restores `autobahn.previous`, but the old bundle is already gone. The rolled-back controller then uploads agents from the *new* bundle, and every handshake fails on hosts of another platform.
- **M-41: the wrong binary can pass the check.** `autobahn install` registers `std::env::current_exe()` as the service's executable (`src/service.rs:98`). `autobahn update` installs into `bin_dir`, which defaults to `~/.local/bin` (`src/update.rs:64`). A service installed from somewhere else, such as a Homebrew path, `target/release`, or a copy in `/Applications`, restarts its *old* executable. `confirm_running` (`:310-322`) only checks that the service reports running, so the update reports success while nothing changed.
- **M-21: fragile service definitions.**
  - `--config` and `--state-root` are stored exactly as typed, so a relative path is later resolved against the service's working directory, not the one it was typed in (`src/service.rs:100-106`).
  - On Linux, `ExecStart` is built by joining arguments with spaces, and `Environment=` wraps an unescaped value in quotes (`:427-446`). A space in a path splits it into two arguments. A `"` breaks the quoting. A `%` is expanded by systemd. A newline adds a directive of the caller's choosing.
  - `start` and `restart` validate the default config, not the `--config` written into the unit (`src/main.rs:595`, `:606`), so they can pass a config the service will reject.

  The macOS plist path already escapes its XML correctly (`:186-190`).

## Proposed resolution

- **M-40: keep the old bundle until the new version is confirmed.** `refresh_agents` moves the old bundle to `agents.previous` and leaves it there. `restart_service` treats `agents.previous` like `autobahn.previous`:
  - on success, it deletes both;
  - on failure, it restores both, bundle first.

  `--no-agents` updates keep the current bundle untouched, as today.
- **M-41: update what the service runs, and check its version.**
  - Read the registered executable back from the unit or plist, and compare it with the resolved install target.
  - If they differ, ask. By default, refuse, with "the login service runs <path>; run `autobahn install` from the new binary, or pass `--retarget` to point the service at <target>". `--retarget` rewrites the service definition.
  - After restarting, ask the service for its build over the control socket; control requests already carry a build (`9dc4a88`). Compare that with the new version, not just the running state. A wrong version triggers the rollback, as a failure to start does.
- **M-21: write service definitions properly.**
  - Make `--config` and `--state-root` absolute at `install` time.
  - Write each systemd `ExecStart` argument in systemd's quoted form: double quotes, with `\` and `"` escaped and `%` doubled.
  - Refuse any path or `AUTOBAHN_HOME` containing a newline.
  - Escape `%`, `\` and `"` in the `Environment=` value.
  - `start` and `restart` validate the config the installed service will read. Take it from the unit or plist, not from the default path.

## Tests

- **M-40:** using the fake fetcher seam in `update::run` and a stub service whose restart fails, check that after the update both the binary and the bundle are the previous ones.
- **M-41:**
  - a service registered at a different path is refused without `--retarget`;
  - with `--retarget`, it is retargeted and confirmed;
  - a stub service that reports an old build triggers the rollback.
- **M-21:**
  - a generated unit, for a config path containing a space, `%`, `"` and `$`, passes `systemd-analyze verify` in CI's Linux job, when available, and round-trips through a small `ExecStart` parser in a unit test;
  - a relative `--config` is stored absolute;
  - a newline in the path is refused;
  - the plist output for the same paths is well-formed XML.
