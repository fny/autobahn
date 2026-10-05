# Release Management & In-Place Updates

This document describes the distribution architecture, release validation pipeline, and update lifecycle for Autobahn.

## Distributed Artifact Components

A standard release publishes these artifacts:

| Artifact | Source Target | Destination Path |
| :-- | :-- | :-- |
| **Controller Executable** | `cargo build --profile dist` | `~/.local/bin/autobahn` |
| **Remote Agents Bundle** | `scripts/build-agents.sh` | `~/.autobahn/agents/` |
| **macOS Menu Bar App** | `apps/tray/build.sh` | `/Applications/Autobahn Tray.app` |
| **macOS Desktop App** | `apps/app/build.sh` | `/Applications/Autobahn.app` |
| **Linux Desktop App** | `cargo build --features app` | wherever you extract it |

## Release Pipeline Architecture

Pushing a version tag (`v*`) triggers `.github/workflows/release.yml`:

1. **Gate Verification:** Compares the tag against `version` in `Cargo.toml` and confirms that all required CI matrix jobs (`linux`, `linux-arm`, `mac`, `spec`) passed on the tagged commit.
2. **Binary Compilation:** Compiles static musl binaries for Linux (`x86_64`, `aarch64`) and native macOS binaries.
3. **macOS Notarization:** Imports Developer ID certificates, signs the command-line binaries and both application bundles, and submits each to Apple's notarization service.
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
RWS3HbhwCUSo45VKntoj+uWQrIS5m8FtkPruyFUuc3xYpNomDsfoSa45
```

To manually verify downloaded release artifacts:

```sh
# Download release manifests
gh release download v1.0.0 --repo fny/autobahn --pattern 'SHA256SUMS*'

# Verify signature
minisign -V -p release.pub -m SHA256SUMS

# Verify binary digests
sha256sum -c --ignore-missing SHA256SUMS
```

## Signing and Notarising (macOS)

A downloaded app must be signed with a Developer ID certificate and notarised — scanned by Apple, with the verdict stapled inside the bundle so Gatekeeper trusts it offline. A copy that arrives by `scp`, or through Autobahn itself, is never quarantined and needs none of this.

`apps/tray/release.sh` does the whole thing, on a laptop or in CI, for either app:

```sh
apps/tray/release.sh                                     # build, sign, notarise, staple
apps/tray/release.sh --sign-only "apps/tray/Autobahn Tray.app"  # sign a bundle already built
apps/tray/release.sh --sign-only "apps/app/Autobahn.app"
```

It signs whatever bundle it is given: the executable to check and to sign is the one `CFBundleExecutable` names, not a fixed `autobahn`. Any other Mach-O in `Contents/MacOS` is signed first, inner out — signing a bundle reaches its main executable and its resources and nothing else, and notarisation refuses the bundle for an unsigned neighbour.

`--sign-only` compiles nothing: it takes a bundle `build.sh` already assembled and signs it, replacing whatever signature was there. That split is what CI uses, so every build happens before the signing identity exists.

On a laptop it signs with the Developer ID certificate in your keychain and notarises with credentials stored once:

```sh
xcrun notarytool store-credentials autobahn \
    --apple-id you@example.com --team-id TEAMID --password <app-specific>
```

### In CI

Pushing a `v*` tag runs `.github/workflows/release.yml`, whose `mac` job builds and signs everything macOS on one runner: the two command-line binaries, signed and notarised by `apps/tray/notarize-cli.sh`, and both apps — the window as `Autobahn-macos-aarch64.zip` and the menu bar one as `Autobahn-Tray-macos-aarch64.zip`. Each is notarised on its own submission, because a ticket is stapled to one bundle.

The `app-linux` job builds the window app for Linux on native `x86_64` and `aarch64` runners, as `Autobahn-linux-x86_64.tar.gz` and `Autobahn-linux-aarch64.tar.gz`. These are unsigned, since Linux has nothing like Gatekeeper, but they are in `SHA256SUMS` with everything else, so the minisign signature covers them.

The job builds everything first — the binaries and both bundles — and checks that each `Info.plist` reports the tag's version. Only then does it import the certificate and sign with `release.sh --sign-only`, so no dependency's build script or proc macro ever runs while the identity is usable.

It is the only job holding the certificate, and it uses the protected `release` environment, which must hold five secrets:

| secret | what it is |
| :-- | :-- |
| `DEVELOPER_ID_P12` | the Developer ID Application certificate and its private key, exported as a `.p12` and base64-encoded |
| `DEVELOPER_ID_P12_PASSWORD` | the password the `.p12` was exported with |
| `NOTARY_API_KEY` | an App Store Connect API key, the contents of its `AuthKey_….p8` file |
| `NOTARY_KEY_ID` | that key's ID — the part of the filename after `AuthKey_` |
| `NOTARY_ISSUER_ID` | the issuer ID shown above the key list in App Store Connect → Users and Access → Integrations |

The certificate can sign anything as you, so it is kept where it can do the least harm:

- The environment requires approval, so a release waits for you before any secret reaches a runner.
- Pull requests from forks never receive secrets, and the job runs only on tags, which only people with write access can push.
- The certificate goes into a throwaway keychain, deleted when the job ends, whether it passed or failed.

If the certificate ever leaks, revoke it in your Apple Developer account.

On macOS, both apps are Apple Silicon only; the command-line binaries cover Intel as well. A command-line binary cannot be stapled, so Gatekeeper checks its notarisation online the first time it runs. The runner's default Xcode may be older than 26, whose `actool` is the only one that compiles the Icon Composer bundle; the job picks Xcode 26 when the runner has it, and otherwise `build.sh` uses the committed `assets/autobahn.icns`, the same icon without the macOS 26 variants.

## See Also

- [Installation](../INSTALL.md): Initial setup and the first synchronization
- [Commands](./commands.md#manage-the-service-and-updates): Service management and updates
- [Development](./development.md): Local builds and required checks
- [State](./state.md): Agent bundles, deployment, and compatibility epochs
- [Menu Bar Item](./tray.md): The standalone tray app
- [Desktop App](./app.md#download): Desktop app downloads
