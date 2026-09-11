# Autobahn documentation

One page per topic. Every page links the ones it leans on, and this index
lists them all. The [README](../README.md) is the front door: what
autobahn is, how to install it, and the minimum to get a group syncing.

## Using it

| page | what it covers |
|---|---|
| [Configuration](./configuration.md) | every key in `~/.autobahn/config.toml`, where it lives, what it defaults to |
| [Modes](./modes.md) | the four sync modes, what each does case by case, and which to pick |
| [Ignores](./ignores.md) | pattern semantics, ignore files, negations, and what "ignored" does not protect |
| [Alerts](./alerts.md) | the one hook, what it receives, and the rules for when it fires |
| [Commands](./commands.md) | asking a running supervisor things; what the state words mean; one-off syncs |
| [Conflicts](./conflicts.md) | `issues`, `conflicts`, `diff`, `resolve` — and how resolution actually works |
| [The shop](./shop.md) | `autobahn mi`: watch it work, and clear the queue from a tree you can act on |
| [The menu bar app](./macos-app.md) | an icon in the colour of the worst session; signing and notarising |
| [The log](./logging.md) | levels, what `debug` adds, rotation |
| [State](./state.md) | what lives in `~/.autobahn`, `clean`, agents and compatibility epochs |

## Understanding it

| page | what it covers |
|---|---|
| [Safety rules](./safety.md) | what a cycle does, and the deletions and overwrites that are refused |
| [Overlapping and nested roots](./nesting.md) | what is refused and why, and how to ignore an inner root |
| [Scope and support boundaries](./support-boundaries.md) | platforms, filesystems, and the cases outside the guarantees |
| [How autobahn works](./HOW-IT-WORKS.md) | the design: the problem, the decisions, and what they cost |
| [Why mutagen is slower](./MUTAGEN.md) | where the memory and latency difference comes from, in mutagen's code |

## Working on it

| page | what it covers |
|---|---|
| [Development](./development.md) | building, targeted tests, the A/B gate, compatibility epochs |
| [`correctness/`](./correctness/) | the invariants, the code that enforces each, the tests, and the residuals |
| [`reviews/`](./reviews/) | adversarial reviews of specific subsystems |
| [`../BENCHMARK.md`](../BENCHMARK.md) | the published comparison against mutagen |
| [`../bench/README.md`](../bench/README.md) | the benchmark harness |
