# MAC-3: A halted session alerts about a minute after it halts, not two

**Findings:** new, from MAC-BENCH 5. **Status:** proposed, Low.

## Problem

`MAC-BENCH.md` section 5 says a missing alpha "alerts only after two minutes, since a drive often comes back with the wake". It does not. The hold for a halt is zero:

```rust
// src/config.rs:233
Alert::Halted => Duration::ZERO,
```

What delays the notification is `DEFAULT_COALESCE_AFTER` (60 s), which exists to gather a cascade, not to wait out a transient. Measured on macOS 26.5.1, Apple M4, commit `d7c2e21`, 2026-09-24, with `on_alert` writing a timestamped line: volume detached at **00:10:04**, exactly one notification at **00:11:05** — 61 seconds.

Why it matters beyond the wording: a laptop that sleeps and wakes is the common case, and this is the same class of problem that `a940438` fixed for connections, where a closed lid paged its owner about nothing. A drive that is briefly absent at wake now pages after a minute, while an unreachable *host* is given five.

A second, smaller mismatch found in the same run: the halt **cleared itself** when the volume came back 33 seconds later, with no notification and no human action. `docs/safety.md:58` says "A halt needs a person; retrying never clears it." One of the two is wrong.

## Proposed resolution

- Give `Alert::Halted` a non-zero hold, or split the state: a root that is *absent and may return* (an unmounted volume, a sleeping laptop) is not the same as a root that was *emptied and decided*, and only the second deserves a zero hold.
- Correct `docs/safety.md` either way: say which halts clear by themselves when the condition goes away, and which need a person.

## Tests

- A halt that clears inside the hold produces no notification at all.
- A halt that outlasts the hold produces exactly one.
- The hold is the state's own, not the coalesce window: with coalescing set to zero, a halt still waits.
