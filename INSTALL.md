# Set up Autobahn with an LLM

Give this file to your coding agent and ask it to set up Autobahn. The instructions below are for the agent.

Your goal is one working sync group. Inspect what you can, ask one short question at a time for missing information, and run the commands yourself when you have terminal access. Use answers already given. Explain each meaningful choice briefly; don't walk the user through every setting.

## 1. Find out what to sync

Check the OS, architecture, whether `autobahn` is installed, and whether it already has a configuration or running supervisor. Preserve existing groups and session state. The default configuration is `~/.autobahn/config.toml`; respect an existing `AUTOBAHN_HOME` or custom service configuration.

Ask: **“Which folder should sync, and to which machine and folder?”** A destination can also be another local folder. Get real paths and an SSH host or alias; don't use the example values below literally. Check whether either folder already contains files.

Recommend `two-way-conflict` for editing on both sides. Explain that changes, including deletions, travel both ways, while conflicting edits require a choice. If the user wants a one-way copy, consult [Modes](docs/modes.md) before choosing: a strict mirror can delete destination-only files.

## 2. Install and check access

Release binaries support Linux x86-64/arm64 and macOS Intel/Apple Silicon. Run as the ordinary user, without `sudo`. If Autobahn is missing:

```sh
curl -fsSL https://github.com/fny/autobahn/releases/latest/download/install.sh | sh
```

The installer puts the CLI in `~/.local/bin`, the agent bundle in `~/.autobahn/agents`, and creates a configuration template if needed. Follow its PATH guidance, then check `autobahn --version`. It does not start syncing or register a service. See [Development](docs/development.md) only if a source build is needed.

For a remote destination, test key-based SSH using the chosen host:

```sh
ssh -o BatchMode=yes -o ConnectTimeout=10 user@build.example.com true
```

If access fails, help the user fix authentication or verify the host key before continuing. Don't bypass host-key checks. Autobahn installs its matching remote agent automatically; no separate remote CLI installation is needed.

## 3. Write a small configuration

If there is no configuration, `autobahn init` creates a template. Edit the existing file without `--force`. Add a uniquely named group using the agreed paths, mode, and exclusions:

```toml
[groups.project]
alpha = "~/project"
betas = ["user@build.example.com:/srv/project"]
mode = "two-way-conflict"
ignores = [".git", ".DS_Store", "node_modules", "target"]
```

Make sure the alpha folder exists. For a local destination, use an absolute path or one beginning with `~/` or `./`.

Tailor the exclusions to the project. Ask whether Git history should sync if relevant; `.git` is excluded in the generated defaults. Group ignores **append** to defaults, and `.gitignore` is not read automatically. See [Configuration](docs/configuration.md) or [Git checkouts](docs/git.md) only as needed.

Before saving, state the selected roots, direction, and exclusions. A running supervisor normally applies configuration edits automatically, so saving can start the new group immediately.

## 4. Verify the first sync

For a fresh setup with no running supervisor:

```sh
autobahn sync
autobahn status project
```

Replace `project` with the chosen group name. Bare `sync` runs **all** configured groups; check the scope on an existing setup. If a supervisor is already running, let it reload, then use `autobahn flush project` and inspect status instead of starting a competing sync. If live reload is disabled, arrange a restart with the user.

Check the command result and actual files at the destination. Report conflicts or blocked paths instead of claiming success or choosing a winner automatically. Use [Commands](docs/commands.md) and [Conflicts](docs/conflicts.md) if troubleshooting is needed.

## 5. Choose how it keeps running

Ask: **“Should Autobahn keep syncing automatically at login, or only when you run it?”** Keep an existing service unless the user wants a change.

- **At login:** `autobahn install` registers and starts a launchd agent on macOS or a systemd user service on Linux. Keep the same configuration and state paths.
- **In a terminal:** `autobahn watch` keeps syncing until Ctrl-C. Run it in the user's terminal or a persistent session.
- **On demand:** use `autobahn sync` for each pass.

Stop any foreground watcher before starting the service. Check `autobahn status` afterward. Offer [Autobahn Dash](docs/app.md) or the [macOS tray](docs/macos-app.md) only if the user wants a desktop interface.

Finish with a short handoff: the configuration path, synced folders, mode, verification result, and whether syncing is running. For a service, mention `autobahn stop` to stop it and `autobahn start` to resume.
