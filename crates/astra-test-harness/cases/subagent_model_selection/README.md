# DeepSeek Flash subagent selection harness

These cases use the existing `astra-test` CLI harness and a live Server and
DeepSeek Flash route. They never contain credentials or a real Offering ID.

Run against a clean candidate build whose CLI and Server report the same Git
revision. Configure the Server, profile and model credentials through the
normal Astra setup, then run:

```sh
astra-test --suite crates/astra-test-harness/cases/subagent_model_selection \
  --models deepseek-v4-flash --no-judger --parallel 1 --runs 3
```

For revision-bound evidence set `ASTRA_EXPECTED_BUILD_GIT_SHA` to the full
candidate commit SHA and follow the harness preflight instructions in
`crates/astra-test-harness/README.md`. The valid case asserts exactly two
spawn events, one per fanout slot, and checks that both expose the prepared
`deepseek-v4-flash` selection identity in the journal. It also checks that the
prepared child run IDs are distinct and match the IDs returned by that fanout
call. The invalid final-slot case must show zero `agent_spawned` events, not
just a failed terminal answer.

These cases deliberately do not call `reflect` or add model-request-ledger
reads. They provide real-provider execution and child-result evidence, plus
the prepared selection identity; they do not independently assert the final
provider request identity for each child. If that attribution is needed,
perform an explicit Reflect audit separately and report its database reads,
extra model turn, latency, and cost outside these measurements. Save the report
and structured journal for cost analysis, and report missing usage as unknown
rather than inferring cost from a passing answer. To keep the complete harness
summary and per-case artifacts after the command exits, run:

```sh
run_dir="$(mktemp -d "${TMPDIR:-/tmp}/astra-subagent-selection.XXXXXX")"
printf 'Harness artifacts: %s\n' "$run_dir"
astra-test --suite crates/astra-test-harness/cases/subagent_model_selection \
  --models deepseek-v4-flash --no-judger --parallel 1 --runs 3 \
  --artifacts-dir "$run_dir/cases" \
  --report-file "$run_dir/report.json" \
  --eval-file "$run_dir/eval.json"
```

The report and case artifacts may contain prompts and model responses. Keep the
directory local, inspect it before sharing, and do not commit it. These output
flags write local files; they do not add database reads or writes.
