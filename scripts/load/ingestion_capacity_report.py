#!/usr/bin/env python3
"""Apply declared single-process ingestion release gates to probe evidence."""

from __future__ import annotations

import argparse
import json
from pathlib import Path
import statistics
import sys


def evaluate(artifact: dict) -> dict:
    result = artifact["result"]
    failures: list[str] = []
    if result["schema_version"] != 1:
        raise ValueError("unsupported evidence schema")
    def count(mapping: dict, key: str) -> int:
        value = mapping[key]
        if type(value) is not int or value < 0:
            raise ValueError(f"{key} must be a nonnegative integer")
        return value

    for key in ("rate", "warmup_seconds", "measurement_seconds", "scheduled",
                "submitted", "generator_missed", "reconciled_durable",
                "unresolved_observations", "reconciled_unknown_durable",
                "unresolved_at_declared_drain_deadline", "late_terminal_resolutions",
                "resident_events_after_shutdown"):
        count(result, key)
    if result["rate"] == 0 or result["scheduled"] != result["rate"] * (result["warmup_seconds"] + result["measurement_seconds"]):
        failures.append("scheduled arrivals disagree with declared rate and duration")
    if result["submitted"] + result["generator_missed"] != result["scheduled"]:
        failures.append("submitted and missed arrivals do not account for scheduled work")
    if not artifact.get("source_unchanged_during_run"):
        failures.append("source changed during run")
    if result.get("scope") != "single_process_ingestion_shared_pool":
        failures.append("unsupported evidence scope")
    if not result.get("measurement_complete"):
        failures.append("incomplete measurements")
    if not result.get("duration_eligible_for_sustained_claim") or result["warmup_seconds"] < 60 or result["measurement_seconds"] < 600:
        failures.append("requires at least 60s warmup and 600s measured arrivals")
    if result["generator_missed"] * 1_000 > result["scheduled"]:
        failures.append("generator missed more than 0.1% of scheduled arrivals")
    counts = result["all"]
    for key in ("committed_inserted", "committed_replayed", "rejected_before_acceptance",
                "rejected_after_acceptance", "unknown_before_acceptance", "unknown_after_acceptance"):
        count(counts, key)
    if any(counts[key] for key in (
        "rejected_before_acceptance", "rejected_after_acceptance",
        "unknown_before_acceptance", "unknown_after_acceptance",
    )):
        failures.append("delivery rejected or durable outcome unresolved")
    if result["unresolved_observations"] or result["reconciled_unknown_durable"]:
        failures.append("unresolved acknowledgement accounting")
    if result["unresolved_at_declared_drain_deadline"] or result["late_terminal_resolutions"]:
        failures.append("delivery resolved only after the declared drain deadline")
    committed = counts["committed_inserted"] + counts["committed_replayed"]
    if committed != result["submitted"] or committed != result["reconciled_durable"]:
        failures.append("offered/committed/durable counts disagree")
    if result["resident_events_after_shutdown"] != 0:
        failures.append("resident work remains after shutdown")
    owners = result["measured_arrivals"]["owner_committed"]
    if any(type(value) is not int or value < 0 for value in owners):
        raise ValueError("owner counts must be nonnegative integers")
    if len(owners) != 100 or not all(value > 0 for value in owners):
        failures.append("at least one owner made no measured progress")
    baseline = result["foreground_baseline"]
    foreground = result["foreground_contended"]
    for workload in (baseline, foreground):
        count(workload, "failed")
        count(workload, "skipped")
        if workload["completion"]["p95"] is not None:
            count(workload["completion"], "p95")
    if baseline["failed"] or baseline["skipped"] or foreground["failed"] or foreground["skipped"]:
        failures.append("foreground baseline or contended workload incomplete")
    baseline_p95 = baseline["completion"]["p95"]
    foreground_p95 = foreground["completion"]["p95"]
    # Declared probe budget, not a product-wide SLO. Quantized upper bounds
    # are compared consistently; the floor avoids amplifying tiny baselines.
    foreground_budget_us = max(10_000, 2 * (baseline_p95 or 0))
    if foreground_p95 is None or baseline_p95 is None or foreground_p95 > foreground_budget_us:
        failures.append("foreground p95 exceeds max(10ms, 2x baseline)")
    for sample in result["samples"]:
        for key in ("elapsed_ms", "resident_events", "oldest_accepted_ms"):
            count(sample, key)
    samples = [sample for sample in result["samples"]
        if result["warmup_seconds"] * 1_000 <= sample["elapsed_ms"]
        < (result["warmup_seconds"] + result["measurement_seconds"]) * 1_000]
    start_ms = result["warmup_seconds"] * 1_000
    duration_ms = result["measurement_seconds"] * 1_000
    times = [start_ms] + [s["elapsed_ms"] for s in samples] + [start_ms + duration_ms]
    # One-second sampling permits scheduling jitter, but never an unobserved
    # multi-second window or a truncated tail masquerading as sustained evidence.
    if any(b < a or b - a > 2_500 for a, b in zip(times, times[1:])):
        failures.append("backlog samples do not cover the measured window with gaps <=2.5s")
    first = [s for s in samples if s["elapsed_ms"] < start_ms + duration_ms / 4]
    last = [s for s in samples if s["elapsed_ms"] >= start_ms + 3 * duration_ms / 4]
    backlog_growth = None
    if len(samples) < 4 or not first or not last:
        failures.append("insufficient backlog samples")
    else:
        backlog_growth = statistics.mean(s["resident_events"] for s in last) - statistics.mean(s["resident_events"] for s in first)
        if backlog_growth > max(10, result["rate"] * 0.1):
            failures.append("backlog grew by more than max(10 events, 0.1s of offered work)")
        if max(s["oldest_accepted_ms"] for s in samples) > 5_000:
            failures.append("oldest accepted work exceeded the declared 5s bound")
    return {
        "eligible_single_process_ingestion_rate": not failures,
        "failures": failures,
        "foreground_p95_budget_us": foreground_budget_us,
        "first_to_last_quarter_backlog_growth": backlog_growth,
        "rate": result["rate"], "distribution": result["distribution"],
        "scope": "one process, declared ingestion workload and shared pool only",
        "limitations": ["not active agent/provider turn capacity", "not cluster-wide fairness",
            "repeat the highest passing rate before claiming capacity", "fault recovery is a separate required scenario"],
    }


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("evidence", type=Path)
    args = parser.parse_args()
    try:
        report = evaluate(json.loads(args.evidence.read_text()))
    except (OSError, ValueError, KeyError, TypeError, AttributeError) as error:
        report = {"eligible_single_process_ingestion_rate": False,
                  "failures": [f"malformed evidence ({type(error).__name__})"]}
    print(json.dumps(report, indent=2))
    sys.exit(0 if report["eligible_single_process_ingestion_rate"] else 1)


if __name__ == "__main__":
    main()
