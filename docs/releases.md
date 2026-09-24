# Releases

Autobahn ships three things. They ship independently. A build that refreshes one of them does not refresh the other two.

| what | where it comes from | where it lands |
|---|---|---|
| the controller command | `cargo build --release`, or a release asset | `~/.local/bin/autobahn` |
| the macOS menu bar app | `apps/macos/build.sh`, into `target/tray` | `/Applications/Autobahn.app` |
| the agents | the `autobahn-agents.tar.gz` asset | `~/.autobahn/agents`, then each remote host |

A plain `cargo build --release` gives you a new controller only. The menu bar app keeps the binary it was built with. The agent bundle in `~/.autobahn/agents` keeps the binaries the installer put there.

Remote hosts are different again. The controller runs the agent at `~/.autobahn/bin/autobahn-<version>-<digest>` on the host, and uploads one only if that path fails to run. `<version>` is the package version plus the compatibility epoch, which `protocol::version()` writes as `0.4.0+e12`; `<digest>` is the first twelve hex digits of the binary's blake3. So a host takes a new agent whenever the bytes the controller would send change — a new release, or a rebuild at the same version — and never the same bytes twice.

## The release pipeline

A tag that starts with `v` starts `.github/workflows/release.yml`.

1. A guard job compares the tag with the `version` key in `Cargo.toml`. If they disagree, the run stops before any build starts.
2. The Linux job builds static musl binaries for x86-64 and arm64.
3. The macOS job builds both macOS binaries and the menu bar app. It signs them with the Developer ID certificate. Apple notarises them. This job waits for approval, because it holds the secrets.
4. The release job collects every `autobahn-<os>-<arch>` binary into `autobahn-agents.tar.gz`. It writes `SHA256SUMS` over every asset.
5. `gh release create` publishes the binaries, the bundle, the app archive and the checksums.

The guard job exists because nothing checked the tag before. The package version names the agent on every remote host. If the tag and the package version disagree, the release publishes binaries whose agents carry a different version than the tag. No handshake catches this, because both ends of a session still agree with each other.

`scripts/install.sh` installs from a published release. It maps the platform the same way the controller does. It verifies every download against `SHA256SUMS`. It writes to a temporary file and renames the file into place. It puts the command on your PATH and the agent bundle in `~/.autobahn/agents`.

## Prereleases

Use a prerelease for a build that is not ready for everybody.

| tag | what it means |
|---|---|
| `v0.5.0-dev.1` | a build for a fleet to run, so you learn what real use finds |
| `v0.5.0-rc.1` | a release candidate, which ships as `v0.5.0` if nothing breaks |
| `v0.5.0` | the stable release |

The word for these builds is "prerelease". This project never calls them "alpha" or "beta". Those two words already name the two sides of a sync group. A sentence like "the beta build failed on the beta" has two meanings and no way to pick one.

The release workflow passes `--prerelease` to `gh release create` if the tag holds a hyphen. That is how semantic versioning marks a prerelease. GitHub then keeps "latest" on the last stable release. This matters, because `scripts/install.sh` downloads from `releases/latest/download` by default, and `autobahn update` does the same. A prerelease therefore reaches nobody by accident.

To take a prerelease, ask for it by tag:

```sh
scripts/install.sh --version v0.5.0-dev.1
autobahn update --version v0.5.0-dev.1
```

## How a prerelease reaches the hosts

The agent path holds the package version. Give the prerelease its own package version, and the agent path moves with it.

Set `version = "0.5.0-dev.1"` in `Cargo.toml` and tag `v0.5.0-dev.1`. The agent path becomes `~/.autobahn/bin/autobahn-0.5.0-dev.1+e12`. No host has that file. Each host installs the new agent on its next connection. The old agent stays in place, so a controller that is still on `0.4.0` keeps working.

Do not bump the epoch for this. The version string already changed, and the version string is what the handshake compares.

## The epoch policy

`COMPATIBILITY_EPOCH` in `src/protocol.rs` forces agent replacement when the version string would otherwise stay the same. Bump it if two builds of one package version must not talk to each other. A changed scan rule, a changed ignore rule and a changed wire format are all such cases.

Keep one counter for the whole project. Do not keep one counter per channel.

The handshake compares the whole version string, so a collision between channels could never fool it. The damage is different. A per-channel counter lets a stable release carry a lower epoch than a dev build that already ran on the same hosts. The number then stops meaning "this behaviour is superseded", which is the only thing it is for.

After you bump the epoch, rebuild the agent bundle before you restart the supervisor. A released bundle carries a `MANIFEST` naming the version and blake3 of every binary in it, and a controller refuses to upload a binary its manifest says belongs to another build — before it reaches any host. A bundle you built yourself has no manifest, so a stale one is uploaded and every session fails its handshake, with a message naming the bundle and its age.

## `autobahn update`

`autobahn update` installs a release over the one you are running. It does what the installer does, and it also handles a machine that is already running a supervisor.

```sh
autobahn update                            # the latest stable release
autobahn update --version v0.5.0-dev.1     # a prerelease, by tag
autobahn update --dry-run                  # what it would do, and where
autobahn update --bin-dir ~/bin            # where the command goes
autobahn update --no-agents                # leave the agent bundle alone
```

The order of the steps is the point of the command:

1. Download the platform binary, the agent bundle and `SHA256SUMS` into a temporary directory.
2. Verify every file against `SHA256SUMS`.
3. Run the downloaded binary from the temporary directory and read the version it reports. Ask it which baseline formats it reads: if some session's baseline is in another, the new build will rebuild it from the two sides, which is safe only where they match — so if any such session is not settled (synchronized, no conflicts, nothing blocked), stop here and name it.
4. Replace the agent bundle. Write a temporary directory, then rename it into place.
5. Rename the new binary into place. Keep the old one as `autobahn.previous`.
6. Restart the login service, if one is installed.
7. Confirm that the service came back. If it did not, put `autobahn.previous` back and restart again.

Each step guards against one failure:

- Step 2 runs before anything moves. A checksum that you check after the file is in place is a report, not a guard.
- Step 3 catches a release that published the wrong asset under this platform's name. Such an asset matches its own checksum. Its second half catches an upgrade across a baseline format while two sides still differ: rebuilt then, a baseline would bring back deletions. A build from before the question existed cannot answer it and is let through.
- Step 4 runs before step 6, always. If the controller restarts on a new version while the bundle still holds the old binaries, it uploads an agent named for the new version whose bytes are the old one. Every host on another platform then fails its handshake.
- Step 5 renames, and never writes over the binary in place. A running process holds the inode of the file it started from. A write into that file kills the running service. A rename leaves the old inode alone until the service restarts.
- Step 7 is the only proof that the new version runs here. A restart command returns as soon as the service manager accepts it.

If no login service is installed, the command says so and skips the restart. Start the supervisor yourself with `autobahn watch`, or register a service with `autobahn install`.

## See also

- [Development](./development.md) — building, targeted tests, epochs
- [State](./state.md) — `~/.autobahn`, agents, and the version handshake
- [The menu bar app](./macos-app.md) — building and signing the app
- [Commands](./commands.md) — everything the command line offers
