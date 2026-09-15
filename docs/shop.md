# The shop

```sh
autobahn mi
```

An easter egg that turned useful. Every session is an order, an order
fills as its transfer does, and the shop is open when a supervisor
answers and shuttered when none does. Every number on it is real — it
reads the same `status --json` document as everything else.

```
  ◉ OPEN   🥖 AUTOBÁNH MÌ   15 customers · 12,480 files · 3.4 GB · 1 filling · 2.1 MB/s

  ▸ voltai   → fny     🥖[▓▓▓▓▓░░░░░░░]  served    filling · 1,204 of 8,530
    vibe     → boite   🥖[▓▓▓▓▓▓▓▓▓▓▓▓]  disputed  2 waiting
    voltai   → boite   🥖[▓░░░░░░░░░░░]  disputed  checking the pantry · 14s · 1 waiting
```

An order is always *something* — served, disputed, out of stock — and
sometimes also *doing* something. The first has the coloured word and
never gives it up. The second has a column of its own, filled only once
the work has gone on long enough to be worth mentioning: the same rule
`status` applies, so a routine scan is never announced and one that
drags names itself without displacing the outcome.

Press `?` for a page that explains every word on the screen.

## The counter

The useful half. `ret` opens any order — where it syncs from and to, its
mode, how many cycles it has run and how much it has carried — and then
its issues as a tree: cause, then place, then path. Every level of that
tree can be acted on, so one keypress settles a whole directory or a
single file. `spc` marks a level; mark as many as you like and one
keypress settles all of them together.

```
  ┌──────────────────────────────────────────────────────────────┐
  │ the counter  voltai → fny.voltai.party            3 waiting  │
  │ ▾ 2 conflicts                       both sides changed these │
  │     happy                                    deleted on ours │
  │     voltagen                                 deleted on ours │
  │ ▸ 1 blocked on alpha                       unicode collision │
  │ ▾ 20 blocked on beta            Permission denied (os error) │
  │   ▾ azure/backend/.ruff_cache/0.9.10/                     16 │
  │       10497280429343070344                                   │
  │   ▸ arcturus/frontend/apps/web/public/static/              4 │
  └──────────────────────────────────────────────────────────────┘
```

## Keys

| key | does |
|---|---|
| `↑` `↓` | move |
| `ret` or `→` | open an order, or a branch of its tree |
| `←` | close a branch, then the counter |
| `o` | keep ours |
| `t` | keep theirs |
| `b` | keep both |
| `spc` | mark a dispute, to settle several together |
| `c` | copy the fix for a blocked path to the clipboard |
| `f` | rush an order (flush it now) |
| `?` | help |
| `q` | close the shop |

Each of `o`, `t` and `b` asks before it acts, because resolution
overwrites a file someone edited on every destination in the group. It
then runs the same `resolve` you would type, on whichever paths the
selected level covers — or on every marked level at once, as a single
command. That is also the faster way round: resolution reads each losing
side once per invocation, so twenty paths settled together cost one scan
and twenty settled one by one cost twenty. Marking a folder and a file
inside it is safe; the file is named once. The marks are forgotten once
the settlement runs, and when you leave the counter. Blocked paths autobahn cannot clear itself, since
the commands are `sudo` over ssh and a password prompt has nowhere to
appear — so `c` copies the fix instead.

Under the counter, the last few lines the supervisor wrote — the only
view of the log there is.

## From a notification

`$AUTOBAHN_OPEN`, handed to the alert hook, is a ready command that opens
the shop on the full detail. A notification holds one line; this is the
way from that line to the rest of it.

## See also

- [Conflicts](./conflicts.md) — the `resolve` the shop runs
- [Alerts](./alerts.md) — `$AUTOBAHN_OPEN`
- [Commands](./commands.md) — `status --json`, which the shop reads
