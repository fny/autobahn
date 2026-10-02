# TUI (Experimental)

```sh
autobahn mi
```

A useful easter egg. The Autobahn Mi shop preseents you your sessions as orders managed by the supervisor.

```
  ◉ OPEN   🥖 AUTOBÁNH MÌ   15 customers · 12,480 files · 3.4 GB · 1 filling · 2.1 MB/s

  ▸ voltai   → fny     🥖[▓▓▓▓▓░░░░░░░]  served    filling · 1,204 of 8,530
    vibe     → boite   🥖[▓▓▓▓▓▓▓▓▓▓▓▓]  disputed  2 waiting
    voltai   → boite   🥖[▓░░░░░░░░░░░]  disputed  checking the pantry · 14s · 1 waiting
```


Each row shows the last cycle’s outcome separately from current work. The phase column appears only after the work passes the same duration threshold as `status`.

Press `?` for definitions of the interface labels.

A `⚠ configuration refused …` line means the supervisor rejected a configuration edit. Sessions continue under the last valid configuration. The notice clears after a valid reload. See [Live reload](./configuration.md#editing-it-while-it-runs).

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

Resolution actions ask for confirmation. They can replace edited files across every destination in the group.

Each action runs the normal `resolve` command for the selected paths or all marked paths together. One invocation scans each losing side once. Resolving twenty paths together avoids nineteen additional scans.

Overlapping selections are deduplicated. Marks clear after resolution and after leaving the counter. Blocked-path repairs can require interactive `sudo` over SSH. The interface copies these commands instead of running them.
