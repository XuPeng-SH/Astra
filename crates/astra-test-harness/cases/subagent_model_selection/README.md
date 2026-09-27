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
by exact configured name (`glm-5.2`). It checks the resolved identity, the
child's completed `LlmRound` linked by run ID, and the result in the parent's
final answer. A `launched` receipt alone does not prove the child ran.
Run it only where that name identifies one authorized active Chat model; if
the account has duplicate names, qualify the source in the case for that
deployment. The invalid final-slot case must show zero `agent_spawned` events,
not just a failed terminal answer.

`flash_spawn_natural_language_glm` is the separate intent-binding check: the
user states a hard `glm-5.2` requirement in ordinary language, while the parent
tool call leaves `requested_model_policy` unset (omitted or explicit `null`).
The child must still be admitted and make its provider request using GLM. This
distinguishes server-side extraction of authenticated user intent from merely
echoing a model selector supplied by the parent model. The server's offline
regression tests additionally verify that one user-intent source is assessed
once and a failed assessment is not automatically retried for the same intent.

`flash_natural_user_news_glm` checks the unscripted user journey: the prompt is
only the original Chinese request, including the ordinary spelling `glm5.2`.
It requires an actual GLM child model round, a child web fetch, and a Sina link
in the final answer. This live-news case depends on the public site; report
site/network unavailability separately from model-selection failures.

`flash_semantic_model_reference_glm` isolates candidate-aware semantic
selection from live websites. The user says `5.2glm` without tool syntax;
the case requires the authorized `glm-5.2` child to make a real provider call
and return a small marker. This is one test of a general selector, not a
license to add an alias for that string. Run alongside ambiguous, unavailable,
and source-qualified controls before claiming semantic-selection quality.

`flash_spawn_prohibited_model_fail_closed` is an unhappy-path intent check. A
user prohibition such as “do not use `glm-5.2`” is not a positive model choice;
the runtime must keep it unresolved and block the child rather than silently
inheriting DeepSeek Flash or substituting another model.

`flash_missing_child_model_fail_closed` uses an unavailable fixed identity and
checks that no child starts. A parent may decline the delegation itself or
submit a call that admission rejects; neither path permits a silent fallback.

`flash_fanout_plan_and_high_review` is the fixed-model control journey: one
slot uses GLM-5.2 for a plan-shaped task and another uses DeepSeek Flash with
adaptive high reasoning for a review-shaped task. This pairing follows the
configured wire capabilities: the GLM route exposes a thinking toggle, while
the DeepSeek route exposes an effort-capable thinking protocol. Its
provider-round evidence is linked to the exact child slot and spawn
configuration, so it catches model or reasoning cross-wiring. It is
deliberately a protocol smoke test; it does not claim that the reviewer
consumed the first child's result or that either model produced a high-quality
plan/review.

`flash_fanout_auto_balanced` is a negative control, not evidence that the
router works: it asks for Auto Balanced and verifies that the current product
explains why it cannot route, without silently inheriting the parent model or
starting a child. Auto is intentionally unavailable until comparable
task-level total-cost, quality/reliability, and completion-time evidence exists.
The offline admission test also verifies that a structured Auto tool request
cannot bypass that boundary. This case makes no claim about router quality or
savings.

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
