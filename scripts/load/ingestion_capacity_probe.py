#!/usr/bin/env python3
"""Run the opt-in real-DB ingestion probe and retain only structured evidence."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import time
import uuid

ROOT = Path(__file__).resolve().parents[2]
PREFIX = "INGESTION_CAPACITY_RESULT "
SOURCES = (
    "Cargo.toml",
    "Cargo.lock",
    "crates/services/Cargo.toml",
    "crates/services/src/event_ingestion.rs",
    "crates/services/src/event_ingestion/measurement.rs",
    "crates/services/src/observation_capture.rs",
    "crates/services/src/cancellation_safe_db.rs",
    "crates/services/src/storage.rs",
    "crates/services/tests/ingestion_capacity_db_it.rs",
    "crates/services/tests/common/mod.rs",
    "scripts/load/ingestion_capacity_probe.py",
)


def digest(path: Path) -> str:
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def source_fingerprint() -> dict[str, str]:
    hashes = {name: digest(ROOT / name) for name in SOURCES}
    tracked_diff = subprocess.check_output(["git", "diff", "--binary", "HEAD", "--",
        "crates", "Cargo.toml", "Cargo.lock", "rust-toolchain.toml", ".cargo"], cwd=ROOT)
    hashes["tracked_compile_input_diff"] = hashlib.sha256(tracked_diff).hexdigest()
    return hashes


def positive(value: str) -> int:
    result = int(value)
    if result <= 0:
        raise argparse.ArgumentTypeError("must be positive")
    return result


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--database", required=True, help="Dedicated astra_test_probe_* database; credentials stay in environment/.env")
    parser.add_argument("--rate", type=positive, default=200)
    parser.add_argument("--seconds", type=positive, default=600)
    parser.add_argument("--warmup-seconds", type=positive, default=60)
    parser.add_argument("--baseline-seconds", type=positive, default=10)
    parser.add_argument("--drain-seconds", type=positive, default=60)
    parser.add_argument("--pool-size", type=positive, default=32)
    parser.add_argument("--foreground-rate", type=positive, default=20)
    parser.add_argument("--distribution", choices=("uniform", "hot"), default="uniform")
    args = parser.parse_args()
    if not re.fullmatch(r"astra_test_probe_[A-Za-z0-9_]+", args.database):
        parser.error("database must be a dedicated astra_test_probe_* name")
    env = os.environ.copy()
    env.update({
        "ASTRA_TEST_DB_IT": "1", "ASTRA_DATABASE": args.database,
        "ASTRA_INGESTION_PROBE_RATE": str(args.rate),
        "ASTRA_INGESTION_PROBE_SECS": str(args.seconds),
        "ASTRA_INGESTION_PROBE_WARMUP_SECS": str(args.warmup_seconds),
        "ASTRA_INGESTION_PROBE_BASELINE_SECS": str(args.baseline_seconds),
        "ASTRA_INGESTION_PROBE_DRAIN_SECS": str(args.drain_seconds),
        "ASTRA_INGESTION_PROBE_POOL_SIZE": str(args.pool_size),
        "ASTRA_INGESTION_PROBE_FOREGROUND_RATE": str(args.foreground_rate),
        "ASTRA_INGESTION_PROBE_DISTRIBUTION": args.distribution,
    })
    source_hashes = source_fingerprint()
    print("Building the capacity probe; no raw logs or credentials will be saved.", flush=True)
    build = subprocess.run([
        "cargo", "test", "--locked", "-p", "astra-services", "--features", "capacity-probes",
        "--test", "ingestion_capacity_db_it", "--no-run", "--message-format=json",
    ], cwd=ROOT, env=env, capture_output=True, text=True, check=False)
    if build.returncode:
        print("Probe build failed. Run the cargo build command directly for local diagnostics.", file=sys.stderr)
        return build.returncode
    executable = None
    compiler_profile = None
    for line in build.stdout.splitlines():
        try:
            message = json.loads(line)
        except json.JSONDecodeError:
            continue
        if message.get("reason") == "compiler-artifact" and message.get("target", {}).get("name") == "ingestion_capacity_db_it":
            executable = message.get("executable") or executable
            compiler_profile = message.get("profile")
    if not executable:
        raise RuntimeError("cargo did not report a probe executable")
    binary_hash = digest(Path(executable))
    head = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=ROOT, text=True).strip()
    print(f"Running {args.distribution}: {args.rate} events/s, {args.warmup_seconds}s warmup + {args.seconds}s measurement.", flush=True)
    started = time.monotonic()
    process = subprocess.Popen([executable, "sustained_ingestion_capacity", "--exact", "--ignored", "--nocapture"],
        cwd=ROOT, env=env, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
    result = None
    assert process.stdout is not None
    for line in process.stdout:
        if line.startswith(PREFIX):
            if result is not None:
                raise RuntimeError("duplicate result records")
            result = json.loads(line[len(PREFIX):])
        elif line.startswith("INGESTION_CAPACITY_PROGRESS "):
            print(line.strip(), flush=True)
    code = process.wait()
    if code or result is None:
        print(f"Probe failed (exit {code}); raw output was not retained. No valid capacity result was produced.", file=sys.stderr)
        return code or 1
    unchanged = source_hashes == source_fingerprint()
    artifact = {"git_head": head, "source_sha256": source_hashes, "binary_sha256": binary_hash,
        "source_unchanged_during_run": unchanged, "wall_seconds": time.monotonic() - started,
        "compiler_profile": compiler_profile, "command_parameters": vars(args), "result": result}
    output = ROOT / "target" / "expriment" / "ingestion-capacity" / f"{time.strftime('%Y%m%dT%H%M%S')}-{uuid.uuid4().hex[:8]}.json"
    output.parent.mkdir(parents=True, exist_ok=True)
    with output.open("x", encoding="utf-8") as stream:
        json.dump(artifact, stream, indent=2)
        stream.write("\n")
    print(json.dumps({"evidence": str(output), "source_unchanged": unchanged,
        "submitted": result["submitted"], "generator_missed": result["generator_missed"],
        "committed": result["all"]["committed_inserted"] + result["all"]["committed_replayed"],
        "measurement_complete": result["measurement_complete"]}), flush=True)
    return 0 if unchanged else 2


if __name__ == "__main__":
    raise SystemExit(main())
