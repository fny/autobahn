# Setting up Autobahn

Give this file to your coding agent and ask it to set up Autobahn. The instructions below are for the agent.

Your goal is to set up Autobahn and have one sync group working.

For details see [the repository](https://github.com/fny/autobahn).

## 1. Install Autobahn

Check whether `autobahn` is already on the PATH. If not, run the install script. Release binaries support Linux x86-64/arm64 and macOS Intel/Apple Silicon. Run as the ordinary user, without `sudo`. Install script:

```sh
curl -fsSL https://github.com/fny/autobahn/releases/latest/download/install.sh | sh
```

If `autobahn` does exist, check whether there's an update and ask the user if they want to install it.

When Autobahn installs, it creates a config file in `~/.autobahn/config.toml`. Briefly familiarize yourself with it. Respect an existing `AUTOBAHN_HOME`.

## 2. Managing Syncing

First ask the user which folder they want to sync. Once you have that, move on to determining which hosts the sync should target as replicas.

Syncing leverages hosts in `~/.ssh/config` or other hosts the user can log into with their key. Make sure there's something that can be a sync target there. If not guide the user to add a host. Is there a machine they can already SSH into with their key? Is there a new machine we could set up that way? If they have a machine that's behind a NAT, point them to Tailscale.

If hosts are known, present them to the user and ask which hosts to sync the folder to and what the target directories are.

For sync mode, recommend `two-way-conflict` for editing on both sides. Explain that changes, including deletions, travel both ways, while conflicting edits require a choice. If the user wants a one-way copy, consult [Modes](docs/modes.md) before choosing: a strict mirror can delete destination-only files.

## 3. Check Access

For a remote destination, test key-based SSH using the chosen host:

```sh
ssh -o BatchMode=yes -o ConnectTimeout=10 dev@build.audi.de true
```

If access fails, help the user fix authentication or verify the host key before continuing. Autobahn installs its matching remote agent automatically; no separate remote CLI installation is needed.

## 4. Verify the First Sync

For a fresh setup with no running supervisor:

```sh
autobahn sync
autobahn status project
```

Replace `project` with the chosen group name. Bare `sync` runs **all** configured groups; check the scope on an existing setup. If a supervisor is already running, let it reload, then use `autobahn flush project` and inspect status instead of starting a competing sync. If live reload is disabled, arrange a restart with the user.

Check the command result and actual files at the destination. Report conflicts or blocked paths instead of claiming success or choosing a winner automatically. Use [Commands](docs/commands.md) and [Conflicts](docs/conflicts.md) if troubleshooting is needed.

## 5. Choose How It Keeps Running

Ask: **“Should Autobahn keep syncing automatically at login, or only when you run it?”** Keep an existing service unless the user wants a change.

- **At login:** `autobahn install` registers and starts a launchd agent on macOS or a systemd user service on Linux. Keep the same configuration and state paths.
- **In a terminal:** `autobahn watch` keeps syncing until Ctrl-C. Run it in the user's terminal or a persistent session.
- **On demand:** use `autobahn sync` for each pass.

Stop any foreground watcher before starting the service. Check `autobahn status` afterward. Offer the [Desktop App](docs/app.md) or the [menu bar item](docs/tray.md) only if the user wants a desktop interface.

Finish with a short handoff: the configuration path, synced folders, mode, verification result, and whether syncing is running. For a service, mention `autobahn stop` to stop it and `autobahn start` to resume.
