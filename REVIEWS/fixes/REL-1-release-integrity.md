# REL-1: Release integrity, from a strict installer to signed checksums

**Findings:** M-17 (DEEPSEEK F7, GLM M6, KIMI ABN-M18, OPUS S6, ASTRA), H-28 (GLM H2, DEEPSEEK F8, KIMI ABN-M18, OPUS S6), I-3 (KIMI ABN-I3), L-32 (GLM L7).
**Status:** implemented. Step 1 in `4458981` and `654c3c5` (2026-09-24). Step 2 in `10096af` (2026-09-26): the release job signs `SHA256SUMS` with the `MINISIGN_SECRET_KEY` secret of the `release` environment, the updater verifies it with the tag in the trusted comment, and unsigned releases newer than 0.4.0 are refused. Key ID `984B7A187D5598D3`; a copy of the private key is at `~/.config/autobahn-release-signing/` on the dev instance. The signing step first runs on the next tag.

The two steps protect against different attackers:
- **Step 1** fixes a bug. The installer does less than the docs say and less than the updater does. It can be defeated by anyone who can corrupt or block a single download.
- **Step 2** adds a protection that doesn't exist today. It covers someone who can publish release assets, and so can replace a binary and its checksum together.

## Step 1: `install.sh` verifies everything and fails closed

### Problem

`docs/releases.md:27` says the installer "verifies every download against `SHA256SUMS`." It does not:

- **The agent bundle is never verified.** `scripts/install.sh:161-171` downloads `autobahn-agents.tar.gz`, extracts it and moves it into `~/.autobahn/agents` without checking it. This is the highest-risk asset, because the controller uploads it to and runs it on every remote host. The release does publish a checksum for it, and `autobahn update` does verify it.
- **A failed checksum download installs unverified.** `install.sh:135-148` treats any failure to fetch `SHA256SUMS` as "the release publishes no checksums," and installs with only a warning. That includes a network error, a proxy, or someone blocking just that one file. `autobahn update` refuses in the same case.
- **The updater recommends the weaker path.** When checksums are missing, `src/update.rs:117` says "use scripts/install.sh if this release publishes no checksums."

### Proposed resolution

- **Verify the bundle** against its `SHA256SUMS` entry before extracting, using the same exact-name match the binary check uses.
- **Fail closed.** If `SHA256SUMS` cannot be downloaded, stop with an error. Add `--insecure`, or `AUTOBAHN_INSECURE=1` for the piped form, to install old releases that publish no checksums. Print a clear warning when it is used.
- **Distinguish "missing" from "failed."** Where the download tool allows it, a `404` says the release has no checksums, and anything else is a failed download. Both refuse without `--insecure`, with different messages.
- **Reword the updater's error** to point at `--insecure` and explain the risk, rather than presenting the installer as the fallback.
- **Validate tar members.** Refuse any member with an absolute path or `..` before extracting. Modern `tar` does this already; checking in the script documents it and covers old `tar` versions.
- **Docs.** Make `docs/releases.md:27` true, and document `--insecure`.
- **Serve the installer from the release (L-32).** Publish `install.sh` as a release asset, and point the README one-liner at `releases/latest/download/install.sh` instead of the `main` branch. Users then run the installer that matches the release they get, not whatever `main` holds that day.

### Tests

- Run `shellcheck scripts/install.sh`.
- Run the installer against a fake release directory served locally. It should:
  - refuse a tampered bundle;
  - refuse when `SHA256SUMS` returns an error;
  - install with `--insecure` when `SHA256SUMS` is absent;
  - install normally when everything matches.

## Step 2: Sign `SHA256SUMS` with minisign, verify in the updater

### Problem

`SHA256SUMS` is fetched from the same GitHub release as the files it checks, and nothing is signed. Step 1 makes the installer use those checksums properly. Checksums still cannot stop anyone who can publish or edit release assets, such as someone with a stolen token or a compromised release workflow. That attacker replaces a binary and its checksum together, and every check passes.

The impact is large. `autobahn update` installs the new binary and restarts the service onto it. The controller then uploads the release's agent bundle to every remote host and runs it there. One bad release reaches every controller and every host it syncs with.

The macOS binaries are signed and notarized, but neither installer checks that. The Linux binaries are unsigned.

### Decision

- **Sign with minisign.** It uses one small key pair. Verification needs only the `minisign-verify` crate, which has no dependencies.
- **Keep the private key in the approval-gated GitHub `release` environment,** as the Developer ID key already is. Releasing stays automated. This stops a stolen token or a compromised build step from producing a validly signed release. It does not stop a takeover of the maintainer's GitHub account; an offline key would, at the cost of a manual step on every release. Revisit this if the account's risk profile changes.
- **Verify in the updater.** That is where a trusted, already-installed binary can check the next one.
- **Be honest about the first install.** A first `curl | sh` install cannot be protected by this, because the script itself comes from GitHub. The docs will say so.

### Proposed resolution

- **Release job.**
  - The `release` job, not the `mac` job, signs `SHA256SUMS` with minisign and publishes `SHA256SUMS.minisig` alongside it.
  - The private key is a secret in the `release` environment, and its password is a separate secret.
  - Only the `release` job gets `contents: write`, which is related to M-19.
- **Public key.** Committed in the repo as `release.pub`, compiled into the binary, and printed in `docs/releases.md` so people can check it independently.
- **Updater.**
  - After downloading `SHA256SUMS`, download `SHA256SUMS.minisig` and verify it with the built-in key before comparing any checksums.
  - A missing or bad signature refuses the update.
  - For releases from before signing began, allow the update with a warning only when the target version is older than a `FIRST_SIGNED_VERSION` constant. From that version on, refuse.
- **Installer.** If a `minisign` command is on `PATH`, verify the signature and refuse on failure. Otherwise, print one line saying the signature was not checked and how to check it by hand.
- **Key rotation.** The binary can carry more than one public key. A new key ships in a release signed with the old key before the old key is retired.
- **Docs.** `docs/releases.md` gains a short "What an install trusts" section:
  - A first install trusts GitHub and TLS.
  - Updates also check the maintainer's signing key.
  - To check a download by hand, run `minisign -Vm SHA256SUMS -P <key>`.

### Tests

- The updater accepts a correctly signed test release, and refuses a changed `SHA256SUMS`, a missing signature, and a signature from another key. Use a test key pair and the existing fake-fetcher seam in `update::run`.
- The updater allows an unsigned release older than `FIRST_SIGNED_VERSION` with a warning, and refuses an unsigned newer one.
- The installer refuses a bad signature when `minisign` is present.
- In the release workflow, a dry run on a tag in a fork signs with a throwaway key.
