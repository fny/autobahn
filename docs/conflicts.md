# Conflicts

When two sides disagree about a file, `status` names it and these
commands settle it:

```sh
autobahn issues                     # everything that needs you, grouped by cause
autobahn issues voltai autobahn     # ...under one folder
autobahn conflicts                  # every conflict, with what each side holds
autobahn conflicts ~/project        # ...for whatever group syncs that folder
autobahn conflicts voltai autobahn  # ...to one folder inside the group
autobahn conflicts --depth 1        # roll up: which top-level folders, and how many
autobahn conflicts --filter vulns   # only paths containing "vulns"
autobahn conflicts --filter '*.ts'  # ...or matching a glob, at any depth
autobahn diff ./src/main.rs         # the two sides of a file, as a unified diff
autobahn diff project src/main.rs   # same file, by group and root-relative path

autobahn resolve ./src/main.rs --keep alpha       # my version wins, everywhere
autobahn resolve project src/main.rs --keep boite # boite's version wins, everywhere
autobahn resolve project src/main.rs --keep both  # keep alpha's; the loser is
                                                  # renamed aside as main.rs.boite
autobahn resolve voltai autobahn --keep alpha     # every conflict under one folder
autobahn resolve project a.rs b.rs c.rs --keep alpha  # several at once, one pass
autobahn resolve ~/project --all --keep boite     # every conflict in the group
```

## Naming a winner

A winner is named as `status` names it: `alpha`, or a destination's host
(or path). Its version reaches alpha and every other destination, so one
command settles a conflict across a whole fan-out — including
destinations whose own conflict was with a *third* version.

It asks before it acts, unless you pass `--yes` (`-y`).

## How `resolve` works

What it does is retire the *losing* version, not copy the winning one:
the losing side's copy is removed, or moved aside for `--keep both`, and
the next cycle carries the winner across. That is why it settles a
conflict between a file and a whole directory, which no amount of
copying bytes can do — reconciliation already propagates one side's
content over the other's deletion, for a file, a symbolic link, or a
tree alike.

Two things follow. The removal goes through the same transition path a
cycle uses, so an entry that changed since the command started is
refused and reported rather than destroyed; run the command again to
settle it. And the winner arrives on the next cycle, so the command
flushes the supervisor before returning. Without a supervisor running,
run `autobahn sync` once. Nothing here touches the ancestor.

## Reading a long list

`--depth` turns a long list into a map of where the trouble is — seven
hundred paths under one folder are one fact about that folder — and each
level tells you how to look inside the next. `--filter` takes a plain
word (matched anywhere in the path, ignoring case) or a glob: without a
slash it matches at any depth, with one it is anchored to the root.

## Blocked paths

A blocked path is one the endpoint could not read or write — usually
permissions, sometimes a name the destination filesystem refuses. autobahn
cannot clear those itself, since the fix is typically `sudo` over ssh and
a password prompt has nowhere to appear. `issues` prints the command that
would clear each one, and [the shop](./shop.md) copies it to the
clipboard.

## See also

- [Modes](./modes.md) — which disagreements become conflicts in the first place
- [The shop](./shop.md) — the same `resolve`, from a tree you can act on
- [The menu bar app](./macos-app.md) — the same `resolve`, from a menu
- [Commands](./commands.md) — what the state words mean
