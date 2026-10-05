# SSH Setup

Autobahn talks to remote machines over plain SSH using keys. If you can `ssh` into a machine without typing a password, Autobahn can sync to it.

## Creating Your Key Pair

If you already have a key in `~/.ssh`, like `id_ed25519` or `id_rsa`, you're good to go. Otherwise create one:

```sh
ssh-keygen -t ed25519 -N "" -f ~/.ssh/id_ed25519
chmod 600 ~/.ssh/id_ed25519
```

This makes a passwordless key that visible to your user alone.

## Setting Up a Remote

To reach another machine, copy your public key to it:

```sh
ssh-copy-id dev@build.audi.de                                      # put your key on the machine
ssh -o BatchMode=yes -o ConnectTimeout=10 dev@build.audi.de true   # prove it works without prompts
```

If the last command returns with no output and no prompt, you're done. Add the machine to a group and Autobahn takes it from there.

## Use Your SSH Config

To add aliases or other settings, put them in `~/.ssh/config`. Set them once there and every tool uses them, Autobahn included.

```sshconfig
# ~/.ssh/config

Host build.audi.de
  User dev
  IdentityFile ~/.ssh/audi.pem

Host laptop.bmw.de
  User dev
  Port 2222

Host lab
  HostName 10.0.4.17
  User dev
  ProxyJump bastion.audi.de                # reached through another machine
```

With that in place, the group can just name the host:

```toml
[groups.work]
primary = "~/Workspace"
replicas = [
  "build.audi.de:/home/dev/workspace",     # a host and a path
  "laptop.bmw.de",                         # just a host: uses the primary's path there
  "lab:~/Workspace",                       # an alias works too
]
```

A few things Autobahn sets on its own connections no matter what your config says, because these connections stay open for days:

- **No agent or X11 forwarding.** The remote machine never gets your keys.
- **No port forwards.** A forward whose port is taken would break every reconnect.
- **Keepalives every 15 seconds.** A dead network is noticed in about a minute instead of hanging a sync.
- **No SSH compression.** Autobahn already compresses its stream, so doing it twice only costs CPU.

Everything else — `User`, `Port`, `HostName`, `IdentityFile`, `ProxyJump` — comes from your config.

## Lock Down What a Machine Will Serve

By default an agent will serve any folder your user can read. To limit that, put a `host.toml` on the remote machine:

```toml
# ~/.autobahn/host.toml (on the remote machine)
roots = ["~/Workspace", "/srv/repositories"]
```

Now that machine refuses any sync outside those folders, whatever a controller's config asks for.

## When a Machine Won't Connect

Autobahn reports a host that can't be reached in plain words, in `autobahn status`, the app, and alerts:

| Autobahn says | What happened | Fix |
| --- | --- | --- |
| **refused the key** | The machine rejected your key | `ssh-copy-id` it again, or check `IdentityFile` and `User` in your config |
| **changed its host key** | The machine's identity changed since you last connected | If you rebuilt the machine, run `ssh-keygen -R <host>` and connect once by hand. If you didn't, stop and find out why |
| **does not resolve** | The name doesn't turn into an address | Check the spelling, your DNS, or add `HostName` to your config |
| **is unreachable** | Anything else: a timeout, a firewall, the machine is off | Try `ssh -v <host>` to see where it stops |

A machine that's down doesn't stop the rest. Syncing to it waits, everything else keeps going, and it catches up when it comes back. To take a machine out of every group for a while without editing groups by hand:

```sh
autobahn disable --host build.audi.de    # off everywhere it appears
autobahn enable  --host build.audi.de    # and back, picking up where it left off
```

## See Also

- [Configuration](./configuration.md): Groups, replicas, and the rest of `config.toml`
- [Commands](./commands.md): `sync`, `watch`, `status`, `disable`, and `enable`
- [State](./state.md): Where the agent lives on each machine and how upgrades roll out
- [P2P](./p2p.md): Restricted keys for machines that sync to each other
