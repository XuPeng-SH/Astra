"""Offline checks for the capacity gates, including explicit negative controls."""

import copy
import unittest
import json
from pathlib import Path
import subprocess
import sys
import tempfile

from ingestion_capacity_report import evaluate


def evidence():
    foreground = {"failed": 0, "skipped": 0, "completion": {"p95": 2_000}}
    return {"source_unchanged_during_run": True, "result": {
        "schema_version": 1, "unresolved_at_declared_drain_deadline": 0,
        "late_terminal_resolutions": 0,
        "scope": "single_process_ingestion_shared_pool", "measurement_complete": True,
        "duration_eligible_for_sustained_claim": True, "scheduled": 132_000,
        "generator_missed": 0, "submitted": 132_000, "reconciled_durable": 132_000,
        "all": {"committed_inserted": 132_000, "committed_replayed": 0,
            "rejected_before_acceptance": 0, "rejected_after_acceptance": 0,
            "unknown_before_acceptance": 0, "unknown_after_acceptance": 0},
        "unresolved_observations": 0, "reconciled_unknown_durable": 0,
        "resident_events_after_shutdown": 0, "measured_arrivals": {"owner_committed": [1200] * 100},
        "foreground_baseline": foreground, "foreground_contended": copy.deepcopy(foreground),
        "warmup_seconds": 60, "measurement_seconds": 600, "rate": 200,
        "distribution": "Uniform", "samples": [{"elapsed_ms": seconds * 1000,
            "resident_events": 100, "oldest_accepted_ms": 1000} for seconds in range(60, 660)],
    }}


class CapacityGates(unittest.TestCase):
    def test_missing_cutoff_and_negative_counts_are_invalid(self):
        data = evidence()
        del data["result"]["late_terminal_resolutions"]
        with self.assertRaises(KeyError):
            evaluate(data)
        data = evidence()
        data["result"]["generator_missed"] = -1
        with self.assertRaises(ValueError):
            evaluate(data)

    def test_rate_accounting_and_temporal_coverage_are_required(self):
        for modify in (
            lambda r: r.update(rate=500),
            lambda r: r.update(submitted=131999),
            lambda r: r.update(samples=r["samples"][:4]),
            lambda r: r.update(samples=r["samples"][:200] + r["samples"][210:]),
        ):
            data = evidence()
            modify(data["result"])
            self.assertFalse(evaluate(data)["eligible_single_process_ingestion_rate"])

    def test_cli_exit_status(self):
        for data, expected in ((evidence(), 0), ({}, 1), ({"result": []}, 1)):
            with tempfile.TemporaryDirectory() as directory:
                path = Path(directory) / "evidence.json"
                path.write_text(json.dumps(data))
                result = subprocess.run([sys.executable,
                    str(Path(__file__).with_name("ingestion_capacity_report.py")), str(path)],
                    capture_output=True, text=True)
                self.assertEqual(result.returncode, expected)
                self.assertEqual(json.loads(result.stdout)["eligible_single_process_ingestion_rate"], expected == 0)

    def test_clean_sustained_profile_is_eligible(self):
        self.assertTrue(evaluate(evidence())["eligible_single_process_ingestion_rate"])

    def test_smoke_cannot_be_promoted_to_sustained_capacity(self):
        data = evidence()
        data["result"]["duration_eligible_for_sustained_claim"] = False
        self.assertFalse(evaluate(data)["eligible_single_process_ingestion_rate"])

    def test_source_change_rejection_unknown_and_foreground_regression_each_fail(self):
        for modify in (
            lambda d: d.update(source_unchanged_during_run=False),
            lambda d: d["result"]["all"].update(rejected_before_acceptance=1),
            lambda d: d["result"]["all"].update(unknown_after_acceptance=1),
            lambda d: d["result"]["foreground_contended"]["completion"].update(p95=20_000),
            lambda d: d["result"].update(generator_missed=133),
        ):
            data = evidence()
            modify(data)
            self.assertFalse(evaluate(data)["eligible_single_process_ingestion_rate"])

    def test_backlog_growth_and_age_are_not_hidden_by_eventual_drain(self):
        for field, value in (("resident_events", 1000), ("oldest_accepted_ms", 6000)):
            data = evidence()
            for sample in data["result"]["samples"][-150:]:
                sample[field] = value
            self.assertFalse(evaluate(data)["eligible_single_process_ingestion_rate"])


if __name__ == "__main__":
    unittest.main()
