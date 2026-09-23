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
call. `flash_spawn_configured_name_glm` exercises a cross-model child selected
by exact configured name (`glm-5.2`). It checks the resolved identity, exact
child result, and a completed `LlmRound` whose `run_id` matches the spawned
child, proving the provider call used GLM rather than only proving admission.
Run it only where that name identifies one authorized active Chat model; if
the account has duplicate names, qualify the source in the case for that
deployment. The invalid final-slot case must show zero `agent_spawned` events,
not just a failed terminal answer.

`flash_fanout_auto_unavailable` exercises the current fail-closed boundary:
the typed Auto request must appear in the tool arguments, return an explicit
unavailable error, and produce zero child start/termination events. It does
not claim Auto routing works; that remains a separate product workstream.

These cases deliberately do not call `reflect` or add model-request-ledger
reads. The configured-name case proves provider-call identity from the existing
typed step-event capture linked to the spawned child run. Save the report and
structured journal for cost analysis, and report missing usage as unknown
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
