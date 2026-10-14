"""Real supervisor/replay entrypoints, including interruption and safe-prefix recovery."""
from collections import Counter
import json
import os
from pathlib import Path
import subprocess
import signal
import sys
import tempfile
import time
import unittest
from unittest.mock import patch

import manual_harness as h

SHA = "a" * 40
SCRIPT = Path(h.__file__).resolve()
SUPERVISOR = SCRIPT.parents[1] / "harness/process_supervisor.py"


def supervised(command):
    return [sys.executable, str(SUPERVISOR), "run", "--owner-pid", str(os.getpid()),
            "--identity", "manual-progress-test", "--", *command]


def row(name, status="passed"):
    checks = [{"type":"judger", "severity":"quality", "score":None,
               "passed":None, "evaluation":"unscored"},
              {"type":"hard_judger", "severity":"hard", "score":None,
               "passed":status == "passed", "evaluation":"reported"}]
    return {"case":name, "model":"model", "run_index":0, "status":status,
            "has_warnings":False, "failure_class":None if status == "passed" else "Unknown",
            "exit_code":0 if status == "passed" else 1, "criteria":checks,
            "steps":[{"step_index":0, "passed":status == "passed", "duration_ms":100, "criteria":checks}],
            "token_usage_coverage":None, "terminal_reported":dict.fromkeys(h.METRICS, 10)}


def events(statuses=("passed", "passed")):
    result = []
    def add(kind, **fields):
        result.append({"schema":h.SCHEMA, "type":kind, "run_id":"cli", "sequence":len(result)+1, **fields})
    add("suite_started", total=len(statuses), commit=SHA)
    for index in range(len(statuses)):
        add("case_queued", case=f"case-{index}", model="model", run_index=0, queue_position=index+1)
    for index, status in enumerate(statuses):
        if status not in ("cancelled", "unavailable"):
            add("case_started", case=f"case-{index}", model="model", run_index=0)
            add("case_progress", case=f"case-{index}", model="model", run_index=0, phase="settled", elapsed_ms=2500)
        add("case_completed", row=row(f"case-{index}", status))
    counts = Counter(statuses)
    add("suite_completed", total=len(statuses), wall_time_ms=5000, **{s:counts[s] for s in h.STATUSES})
    return result


def write_events(root, data, tail=b""):
    (root / "progress.jsonl").write_bytes(b"".join(json.dumps(e).encode()+b"\n" for e in data)+tail)


class ManualHarnessProgressTests(unittest.TestCase):
    def publish(self, root, outcome="success"):
        with patch.dict(os.environ, {"ASTRA_BUILD_SOURCE_GIT_SHA":SHA, "GITHUB_STEP_SUMMARY":str(root / "job-summary"),
                                    "HARNESS_OUTCOME":outcome}):
            code = h.publish(root)
        return code, json.loads((root / "summary.json").read_text())

    def test_terminal_statuses_and_grading_are_preserved_without_full_report(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            write_events(root, events(h.STATUSES))
            (root / "execution.json").write_text(json.dumps({"exit_code":1, "termination":"finished"}))
            # The finalizer must not read private full reports even if present.
            (root / "report.json").write_text("private-sentinel")
            code, summary = self.publish(root)
            self.assertEqual(code, 1)
            self.assertTrue(summary["complete"])
            self.assertEqual(summary["counts"]["completed"], 4)
            self.assertEqual([r["status"] for r in summary["runs"]], list(h.STATUSES))
            self.assertIsNone(summary["runs"][0]["criteria"][0]["passed"])
            self.assertTrue(summary["runs"][0]["criteria"][1]["passed"])
            self.assertFalse(summary["runs"][1]["steps"][0]["passed"])
            self.assertEqual(summary["runs"][0]["case_elapsed_ms"], 2500)
            self.assertIsNone(summary["runs"][2]["case_elapsed_ms"])
            self.assertNotIn("private-sentinel", (root / "progress.safe.jsonl").read_text())
            self.assertIn("failed", (root / "job-summary").read_text())
            write_events(root, events(("passed",)))
            (root / "execution.json").write_text(json.dumps({"exit_code":0, "termination":"finished"}))
            self.assertTrue(self.publish(root)[1]["success"])
            self.assertFalse(self.publish(root, "failure")[1]["success"])  # supervisor failure cannot be hidden
            for invalid in [{"exit_code":{"private":"private-sentinel"}, "termination":"finished"},
                            {"exit_code":False, "termination":"finished"}]:
                (root / "execution.json").write_text(json.dumps(invalid))
                _, summary = self.publish(root)
                self.assertFalse(summary["success"])
                self.assertNotIn("private-sentinel", (root / "summary.json").read_text())

    def test_missing_terminal_truncation_gaps_and_malformed_records_keep_valid_prefix(self):
        data = events()
        end = next(i for i, e in enumerate(data) if e["type"] == "case_completed") + 1
        prefix = data[:end]
        for tail, reason in [(b"", None), (b'{"private-sentinel":', "truncated_record"),
                             (b'{"schema":"private-sentinel"}\n', "invalid_progress"),
                             (json.dumps(data[end]).replace('"case": "case-1"',
                                 '"case":"private-sentinel","case":"case-1"').encode()+b"\n", "invalid_progress"),
                             (json.dumps({**data[end], "sequence":999}).encode()+b"\n", "invalid_progress")]:
            with self.subTest(reason=reason), tempfile.TemporaryDirectory() as tmp:
                root = Path(tmp)
                write_events(root, prefix, tail)
                code, summary = self.publish(root)
                self.assertEqual(code, 1)
                self.assertFalse(summary["complete"])
                self.assertEqual(summary["counts"]["passed"], 1)
                self.assertEqual(summary["counts"]["waiting"], 1)
                self.assertEqual(summary["observation_error"], reason)
                self.assertNotIn("private-sentinel", (root / "progress.safe.jsonl").read_text())
                self.assertIn("Incomplete", (root / "job-summary").read_text())
        for mutate in [lambda e:e.update(sequence=999), lambda e:e.update(private="private-sentinel"),
                       lambda e:e["row"]["criteria"][0].update(score=float("nan")),
                       lambda e:e["row"]["criteria"][1].update(evaluation="unscored"),
                       lambda e:e["row"]["criteria"][0].update(passed=True),
                       lambda e:e["row"]["criteria"][1].update(passed=False),
                       lambda e:e["row"].update(steps={})]:
            replay = h.Replay(SHA)
            for item in data[:end-1]:
                replay.accept(item)
            bad = json.loads(json.dumps(data[end-1]))
            mutate(bad)
            with self.assertRaises((ValueError, TypeError)):
                replay.accept(bad)
        replay = h.Replay(SHA)
        replay.accept(events(("passed",))[0])
        replay.accept(events(("passed",))[1])
        with self.assertRaises(ValueError):
            replay.accept({**events(("passed",))[1], "sequence":3, "case":"excess-case"})

    def stub(self, root, mode):
        fixture = root / "fixture.json"
        data = events()
        end = next(i for i,e in enumerate(data) if e["type"] == "case_completed")+1
        prefix = data[:end] + [data[end]]
        terminal = events(("passed", "cancelled"))
        # The active second case keeps its start fact; cancellation isn't fabricated on hard kill.
        terminal = terminal[:-2] + [data[end]] + terminal[-2:]
        for i,e in enumerate(terminal):
            e["sequence"] = i+1
        fixture.write_text(json.dumps({"prefix":prefix, "all":data, "cancel":terminal}))
        stub = root / "stub.py"
        stub.write_text("""import json,os,signal,sys,time
from pathlib import Path
mode=sys.argv[1]
if '--help' in sys.argv:
    print('--progress-file' if mode != 'unsupported' else 'no supported option')
    raise SystemExit(0)
root=Path(sys.argv[2])
root.joinpath('argv.json').write_text(json.dumps(sys.argv[1:]))
data=json.loads(root.joinpath('fixture.json').read_text())
out=Path(sys.argv[sys.argv.index('--progress-file')+1])
def emit(items):
    with out.open('w') as f:
        for item in items: f.write(json.dumps(item)+'\\n'); f.flush()
print('private-sentinel',flush=True)
if mode=='success': emit(data['all']); raise SystemExit(0)
if mode=='detached':
    pid=os.fork()
    if pid==0:
        os.setsid()
        signal.signal(signal.SIGTERM,signal.SIG_IGN)
        signal.signal(signal.SIGINT,signal.SIG_IGN)
        root.joinpath('detached.pid').write_text(str(os.getpid()))
        while True: time.sleep(.05)
emit(data['prefix'])
if mode=='graceful':
    def stop(sig,frame): emit(data['cancel']); raise SystemExit(0)
    signal.signal(signal.SIGINT,stop)
    signal.signal(signal.SIGTERM,stop)
else: signal.signal(signal.SIGINT,signal.SIG_IGN)
while True: time.sleep(.05)
""")
        # Supervisor probes the executable, so use a thin argv-preserving launcher.
        launcher = root / "stub"
        launcher.write_text(f"#!/bin/sh\nexec {sys.executable} '{stub}' '{mode}' '{root}' \"$@\"\n")
        launcher.chmod(0o700)
        return launcher

    @unittest.skipUnless(sys.platform.startswith("linux"), "CI supervisor requires Linux")
    def test_actual_supervisor_live_output_graceful_stop_and_hard_kill(self):
        for mode, expected, complete in [("success",0,True), ("graceful",124,True), ("stuck",124,False), ("unsupported",2,False)]:
            with self.subTest(mode=mode), tempfile.TemporaryDirectory() as tmp:
                root = Path(tmp)
                launcher = self.stub(root, mode)
                started = time.monotonic()
                result = subprocess.run(supervised([sys.executable, str(SCRIPT), "run", "--root", str(root),
                    "--timeout-seconds", ".6", "--grace-seconds", ".5", "--", str(launcher),
                    "--filter", "hello* $(touch unexpected)"]), capture_output=True, text=True,
                    env={**os.environ, "ASTRA_BUILD_SOURCE_GIT_SHA":SHA}, timeout=5)
                self.assertEqual(result.returncode, expected, result.stderr)
                self.assertLess(time.monotonic()-started, 4)
                self.assertNotIn("private-sentinel", result.stdout+result.stderr)
                code, summary = self.publish(root)
                self.assertEqual(summary["complete"], complete)
                self.assertEqual(code, 0 if mode == "success" else 1)
                if mode != "unsupported":
                    self.assertIn("[harness] PASSED", result.stdout)
                    self.assertIn("counts", result.stdout)
                    self.assertFalse((root / "unexpected").exists())
                else:
                    self.assertFalse((root / "argv.json").exists())
                if mode == "stuck":
                    self.assertEqual(summary["counts"]["running"], 1)
                    self.assertEqual(summary["counts"]["cancelled"], 0)

    @unittest.skipUnless(sys.platform.startswith("linux"), "CI supervisor requires Linux")
    def test_user_cancel_is_forwarded_and_has_a_distinct_nonpassing_exit(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            launcher = self.stub(root, "graceful")
            process = subprocess.Popen(supervised([sys.executable, str(SCRIPT), "run", "--root", str(root),
                "--timeout-seconds", "10", "--grace-seconds", "1", "--", str(launcher)]),
                stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
                env={**os.environ, "ASTRA_BUILD_SOURCE_GIT_SHA":SHA})
            try:
                end = time.monotonic() + 3
                while not (root / "progress.jsonl").exists() and time.monotonic() < end:
                    time.sleep(.05)
                self.assertTrue((root / "progress.jsonl").exists())
                process.send_signal(signal.SIGINT)
                stdout, stderr = process.communicate(timeout=4)
                self.assertEqual(process.returncode, 130, stderr)
                self.assertNotIn("private-sentinel", stdout + stderr)
                _, summary = self.publish(root)
                self.assertEqual(summary["execution"]["termination"], "cancelled")
                self.assertFalse(summary["success"])
            finally:
                if process.poll() is None:
                    process.kill()
                    process.wait()

    @unittest.skipUnless(sys.platform.startswith("linux"), "existing process supervisor uses Linux subreaper")
    def test_existing_supervisor_reaps_separately_grouped_descendants_after_hard_stop(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            launcher = self.stub(root, "detached")
            pid = None
            try:
                result = subprocess.run(supervised([sys.executable, str(SCRIPT), "run", "--root", str(root),
                    "--timeout-seconds", ".6", "--grace-seconds", ".3", "--", str(launcher)]),
                    capture_output=True, text=True, env={**os.environ, "ASTRA_BUILD_SOURCE_GIT_SHA":SHA}, timeout=9)
                self.assertEqual(result.returncode, 124, result.stderr)
                pid = int((root / "detached.pid").read_text())
                with self.assertRaises(ProcessLookupError):
                    os.kill(pid, 0)
                _, summary = self.publish(root, "failure")
                self.assertFalse(summary["success"])
                self.assertEqual(summary["counts"]["running"], 1)
            finally:
                if pid is not None:
                    try:
                        os.kill(pid, signal.SIGKILL)
                    except ProcessLookupError:
                        pass


if __name__ == "__main__":
    unittest.main()
