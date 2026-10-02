# Release Management & In-Place Updates

This document describes the distribution architecture, release validation pipeline, and update lifecycle for Autobahn.

## Distributed Artifact Components

A standard release publishes three independent artifacts:

| Artifact | Source Target | Destination Path |
| :--- | :--- | :--- |
| **Controller Executable** | `cargo build --profile dist` | `~/.local/bin/autobahn` |
| **Remote Agents Bundle** | `scripts/build-agents.sh` | `~/.autobahn/agents/` |
| **macOS Menu Bar App** | `apps/tray/build.sh` | `/Applications/Autobahn.app` |

## Release Pipeline Architecture

Pushing a version tag (`v*`) triggers `.github/workflows/release.yml`:
1. **Gate Verification:** Compares the tag against `version` in `Cargo.toml` and confirms that all required CI matrix jobs (`linux`, `linux-arm`, `mac`, `spec`) passed on the tagged commit.
2. **Binary Compilation:** Compiles static musl binaries for Linux (`x86_64`, `aarch64`) and native macOS binaries.
3. **macOS Notarization:** Imports Developer ID certificates, signs the application bundle, and submits to Apple's notarization service.
4. **Cryptographic Signing:** Generates `SHA256SUMS` across all assets and signs the checksum manifest using `minisign` (`SHA256SUMS.minisig`) against the release key.
5. **Publishing:** Publishes binary archives, agent tarballs, signatures, and installation scripts via GitHub Releases.

### Prerelease Terminology
Autobahn designates early test builds as **prereleases** (`v0.5.0-dev.1`, `v0.5.0-rc.1`). The terms "primary" and "replica" are strictly avoided in version naming to prevent ambiguity with synchronization endpoint roles.


## Automated In-Place Upgrades (`autobahn update`)

`autobahn update` upgrades the local controller executable and agent bundle in place:

```sh
# Upgrade to latest stable release
autobahn update

# Upgrade to a specific prerelease tag
autobahn update --version v0.5.0-dev.1

# Preview update actions without modifying disk
autobahn update --dry-run
```

### Upgrade Execution Lifecycle

To guarantee zero service disruption and atomic rollbacks, `autobahn update` executes an ordered 7-step sequence:
1. **Service Registration Inspection:** Detects the path registered with `launchd` or `systemd`.
2. **Staged Retrieval & Cryptographic Verification:** Downloads assets to `~/.autobahn/tmp/`, validates `minisign` signatures, and verifies SHA-256 digests before touching active binaries.
3. **Compatibility Inspection:** Executes the candidate binary in a subprocess to confirm support for existing on-disk ancestor formats.
4. **Atomic Agent Bundle Swap:** Unpacks new agents to a temporary directory and renames over `~/.autobahn/agents/`, backing up the previous bundle as `agents.previous`.
5. **Atomic Binary Publication:** Publishes the new binary via filesystem rename (`autobahn.previous`), avoiding truncation of running process inodes.
6. **Daemon Restart:** Restarts the background supervisor service via its service manager.
7. **Control Socket Health Handshake:** Queries the restarted daemon over `~/.autobahn/control.sock`. If the service fails to report healthy status within the timeout window, the upgrade rolls back atomically to `autobahn.previous` and `agents.previous`.

## Minisign Release Verification

Autobahn releases are signed with Minisign using public key `release.pub`:

```
RWTTmFV9GHpLmH3sw8KlWiSqiqJK1AUrb9W3+UUi6/ja1uJ/MRGjGBUQ
```

To manually verify downloaded release artifacts:

```sh
# Download release manifests
gh release download v0.4.0 --repo fny/autobahn --pattern 'SHA256SUMS*'

# Verify signature
minisign -V -p release.pub -m SHA256SUMS

# Verify binary digests
sha256sum -c --ignore-missing SHA256SUMS
```
