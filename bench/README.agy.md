# Benchmark Evaluation Harness

This document describes the automated benchmarking harness used to evaluate propagation latency, initial synchronization throughput, and resource consumption (CPU/RAM) between Autobahn and competing synchronization tools across paired AWS EC2 instances.

---

## Quick Start

Execute benchmark commands from the `bench/` directory:

```bash
# Verify harness integrity locally without cloud resources (~6 minutes)
./smoke.sh

# Bake AWS AMI: clones repository corpora, generates partitions, installs tools
python3 orchestrate.py bake --profile <profile> --region <region>

# Execute benchmark matrix
python3 orchestrate.py run --profile <profile> --region <region> \
    --ami <ami-id> --budget 2000 --repeats 10

# Execute targeted test cell (e.g., 5k corpus with 1 editor)
python3 orchestrate.py run --profile <profile> --region <region> \
    --ami <ami-id> --budget 400 --repeats 10 --cells 5k-1

# Aggregate JSONL results into Markdown and CSV tables
python3 aggregate.py results-<run-id>/

# Terminate AWS infrastructure
python3 orchestrate.py destroy --profile <profile> --region <region> --run <run-id>
```

---

## Architectural Principles of Measurement

1. **Monotonic Single-Clock Timing:**
   Latency is measured from the moment a file modification completes on the writing host until an observer on the destination host confirms verified receipt. Both timestamps derive from the writing host's local monotonic clock, eliminating cross-host clock skew.
2. **Digest Verification vs. Filesystem Events:**
   Destination observers confirm propagation only when the destination file's SHA-256 digest matches the payload announced before the write. Partial writes, atomic renames in progress, or transient metadata changes do not produce false acknowledgments.
3. **Deterministic Nonce Payloads:**
   Payload bytes are uniquely seeded per job iteration using cryptographic nonces. Previous run data cannot satisfy verification checks.
4. **Harness Floor Calibration:**
   Before running tools, the harness measures baseline network and IPC latency by performing the write-and-acknowledge protocol directly. Measured harness overhead consistently ranges between **0.5 ms and 0.8 ms**.

---

## Test Corpora & Working Set Partitions

All test corpora originate from a single Chromium source repository checkout:
- **`chromium`:** Full source tree (~505,000 files).
- **`sub50k` / `sub50k-b`:** Disjoint subsets comprising ~50,000 files each.
- **`sub5k`:** Representative subset of ~5,000 files.

### Working Set Partitioning
To isolate concurrency scaling from cache locality effects:
- The measuring agent edits an identical set of 40 files regardless of whether 1, 10, or 100 concurrent background agents are running.
- Background agents operate on strictly disjoint file partitions.
- Symbolic links are excluded from corpora to ensure uniform evaluation across differing filesystem semantics.
