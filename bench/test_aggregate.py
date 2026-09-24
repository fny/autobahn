#!/usr/bin/env python3
"""Local checks that aggregation reports what it claims.

    python3 bench/test_aggregate.py
"""

import contextlib
import io
import json
import os
import sys
import tempfile
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import aggregate  # noqa: E402
import job  # noqa: E402


def report_for(records, plan=None):
    with tempfile.TemporaryDirectory() as directory:
        with open(f"{directory}/pair-0-results.jsonl", "w") as handle:
            handle.write("".join(json.dumps(r) + "\n" for r in records))
        if plan is not None:
            with open(f"{directory}/plan.json", "w") as handle:
                json.dump(plan, handle)
        out = io.StringIO()
        saved, sys.argv = sys.argv, ["aggregate.py", directory]
        try:
            with contextlib.redirect_stdout(out):
                aggregate.main()
        finally:
            sys.argv = saved
        return json.loads(out.getvalue())


def job_records(name, destinations, betas, cold_s):
    spec = {"job": name, "cell": {"name": "cell-1", "betas": betas}, "tools": ["autobahn"]}
    return [
        {"measurement": "job_start", "job": name, "cell": "cell-1", "spec": spec,
         "destinations": [f"dest{i}" for i in range(1, destinations + 1)]},
        {"measurement": "cold_sync", "job": name, "cell": "cell-1", "tool": "autobahn",
         "timings": {"sub5k": {"count_matched_s": cold_s - 1, "digest_verified_s": cold_s,
                               "verified": True}}},
        {"measurement": "job_complete", "job": name, "statuses": {"autobahn": "ok"}},
    ]


class ColdSyncExclusion(unittest.TestCase):
    def test_a_verified_job_with_the_wrong_destination_count_is_left_out(self):
        records = (job_records("clean-r0", 1, 1, 10.0)
                   + job_records("wide-r1", 10, 1, 99.0))
        row = report_for(records)["cold_sync_s"]["cell-1/autobahn/sub5k"]
        self.assertEqual(row["digest_verified"]["n"], 1)
        self.assertEqual(row["digest_verified"]["median"], 10.0)
        self.assertEqual(row["excluded"],
                         [{"job": "wide-r1", "reason": "destination_width_mismatch:10_of_1"}])

    def test_a_run_from_before_the_destination_record_is_judged_by_its_plan(self):
        records = job_records("clean-r0", 1, 1, 10.0) + job_records("wide-r1", 1, 1, 99.0)
        for record in records:
            record.pop("destinations", None)
            if record["measurement"] == "job_start":
                record["pair"] = "pair-0" if record["job"] == "clean-r0" else "pair-1"
        plan = {"jobs": [], "groups": [[2, "c6i.4xlarge"], [11, "c6i.4xlarge"]]}
        row = report_for(records, plan)["cold_sync_s"]["cell-1/autobahn/sub5k"]
        self.assertEqual(row["digest_verified"]["n"], 1)
        self.assertEqual([e["job"] for e in row["excluded"]], ["wide-r1"])

    def test_the_six_contaminated_jobs_of_bench_1789947877_leave_cold_sync(self):
        directory = os.path.join(HERE, "results-bench-1789947877")
        if not os.path.isdir(directory):
            self.skipTest("results-bench-1789947877 is not in this checkout")
        out = io.StringIO()
        saved, sys.argv = sys.argv, ["aggregate.py", directory]
        try:
            with contextlib.redirect_stdout(out):
                aggregate.main()
        finally:
            sys.argv = saved
        report = json.loads(out.getvalue())
        excluded = {e["job"] for row in report["cold_sync_s"].values() for e in row["excluded"]
                    if e["reason"].startswith("destination_width_mismatch")}
        self.assertEqual(excluded, {"chromium-1-patch-r2", "chromium-10-r2",
                                    "chromium-10-bidir-r1", "chromium-1-bidir-r2",
                                    "chromium-1-bidir-r4", "50k-10-r0"})


class RemoteCpu(unittest.TestCase):
    # Two destinations, each burning a steady half core (50 jiffies a
    # second) from different starting counters, sampled once a second.
    @staticmethod
    def host(base, skip=()):
        return [[1000.0 + t + 0.1 * base, 1000, base * 1000 + 50 * t, 1]
                for t in range(11) if t not in skip]

    def cpu(self, hosts, start, end):
        series = job.merge_remote(hosts)
        record = {"phases": {"workload": {"start": start, "end": end}},
                  "series": {"local": [], **series}}
        return aggregate.windowed(record, "workload", "remote")["cpu_percent_of_core"]

    def test_a_host_missing_from_a_middle_sample_gives_the_same_total(self):
        # Windows over the whole series, ending in the gap, and starting
        # in it: phase boundaries fall wherever they fall.
        for start, end in ((1000.5, 1011.0), (1000.5, 1005.5), (1005.5, 1011.0)):
            whole = self.cpu([self.host(1), self.host(2)], start, end)
            gappy = self.cpu([self.host(1), self.host(2, skip=(5,))], start, end)
            self.assertEqual(whole, gappy, (start, end))
            self.assertAlmostEqual(whole, 100.0, delta=1.0, msg=(start, end))

    def test_the_merged_series_never_goes_down(self):
        merged = job.merge_remote([self.host(1), self.host(2, skip=(4, 5, 6))])["remote"]
        jiffies = [row[2] for row in merged]
        self.assertEqual(jiffies, sorted(jiffies))


if __name__ == "__main__":
    unittest.main()
