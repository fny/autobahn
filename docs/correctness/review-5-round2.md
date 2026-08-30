# Round 2

1. **Journal normalization (#1): concede.** This remains the first correctness fix. Make the normalization write durable via temp-file write, file `fsync`, rename, then directory `fsync`; add fault points around each boundary.

2. **Plan identity check/use race (#10): concede.** Resolve the endpoint once and carry that exact resolved identity through validation, pair-lock construction, and endpoint creation. Re-canonicalizing the original path later preserves the race.

3. **Version locking (#2): concede the priority reduction and the mechanism.** If 0.3.0 was never deployed outside destroyed AMIs, this is not a present mixed-fleet hazard. A protocol compatibility epoch in the handshake, bumped whenever safety semantics change, plus a release version bump is sufficient; hashing the whole binary is over-engineering. The reinstall path still needs a test proving that an epoch mismatch replaces a stale agent even when its semver-named install path already exists. Treat this as a release gate, not the next emergency fix.

4. **Shared writable endpoints (#3): concede the proposed interim, with one condition.** Refusal—not merely a warning—is adequate within one `Config::plans()` result if the comparison covers canonical local-path *containment*, not only exact equality. `A` and `A/subdir` are the same concurrency hazard. Reject when an overlapping endpoint is writable in either plan; that conservatively also rejects a one-way relay where one session writes `B` while another reads it. There is no ordinary same-config topology that defeats that rule. Separate processes/config files remain outside it and should be stated as unsupported until locking is widened beyond exact session pairs.

5. **Byte guard (#9): concede.** A global “count OR bytes” halt would turn a legitimate deletion of one large file into a frequent false positive, while not establishing that a mount disappeared. Keep the current count guard, but record both deleted entry count and estimated bytes in telemetry. Explicitly retain “one/few huge files can evade the guard” as a known gap; revisit only with observed incidents, preferably using mount-identity evidence or an opt-in deletion policy rather than a universal byte cutoff.

6. **Config/NFS work before intents: concede.** The config refusal and NFS detection/documentation are cheap containment and should precede the larger intent-record design. They do not make the crash window safe; they reduce exposure while that work is designed.

7. **PLAN marked 21/21: concede; this is itself a process risk.** “Completed” currently reads as “correctness work is settled” despite accepted #1/#10 and unexercised fault paths. Add a clearly named follow-on/open-risks section with those defects, the intent gap, and harness gaps. Do not reopen completed historical tasks; make the remaining gates impossible to mistake for optional polish.

## Revised order

1. Atomic, durable journal normalization plus its fault-injection tests.
2. Freeze and reuse the resolved endpoint identity plus a race regression test.
3. Correct the PLAN with a follow-on/open-risks section.
4. Refuse canonically overlapping shared writable endpoints within one plan set.
5. Add NFS/statfs warnings and document cross-process and network-filesystem limits.
6. Add the compatibility epoch and bump the release version before any push.
7. Design and implement the intent record next work session.
8. Fill the remaining observer, staging, reconnect, and crash-boundary harness gaps.
