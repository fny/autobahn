# F1r notes
- Started from 94a22ae (integration).
- 5bbdd5f: compile fix only — Probe::Unresponsive (from 910ae3f) mapped to RunningBuild::Absent in update.rs running_build. Amended.
- fcef177: main.rs conflict — integration added Unsettled error type (exit 2) at the same spot F1 added refuse_root(); kept both, adjacent. No new Command variants since wave-0, so refuse_root's list is unchanged.
