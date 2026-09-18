# Optional memory judgment backend pilot

Memory candidate relevance and explicit lesson dismissal can use a registered
TypeSafe System One Offering instead of the usual selector LLM. This is an
opt-in deployment setting for evaluation. Unset configuration preserves the
ordinary selector. It does not change main-agent rounds, tool permissions,
memory extraction, or task completion rules.

## Configure

Register one TypeSafe connection and API key. Runtime judgments and
`astra admin model compare` reuse that Offering; no extra environment variable is
needed.

1. Register a `provider: typesafe` model using the existing model management
   flow. Set `base_url: https://api.typesafe.ai`, a pinned model such as
   `jev-1.13.0`, and the provider key in `api_key`. See the commented example in
   `.models.yaml.example`. The registry stores credentials encrypted; CLI/Edge
   receives only Offering identity. Runtime routing uses admin configuration,
   not environment variables. An empty key cannot be used or bound.

2. Run `astra admin model check <name>` to activate the Offering using a typed
   provider connectivity probe, then obtain its exact Offering ID.
3. Run `astra admin config set judgment_offering_id <offering-id>`.
4. Start a new Session to refresh the CLI's cached judgment Offering.

The binding can also select an ordinary LLM Offering. It is an Offering routing
mechanism, not a separate registry or agent lifecycle. The requesting user's
existing model-access policy is enforced; deployment credentials are not made
available to users whose policy forbids them. Provider credentials and active
status are revalidated by the completion boundary for every call.

To disable the pilot, run
`astra admin config unset judgment_offering_id`, then start a new Session.
Existing sessions retain their cached Offering selection. Other server instances
share the admin configuration and model registry through the database.

## Behavior and limits

The existing `/models/memory` catalog defaults to extraction candidates.
`?operation=judgment` selects the optional judgment binding; extraction retains
its original selector chain and excludes TypeSafe. No new execution endpoint is
added. The existing authenticated completion proxy and durable inference ledger
own provider execution, request hashes, deadlines, attempt settlement, and usage.

Requests carry a strict versioned structured judgment: shared JSON evidence and
keyed Noul questions. Memory code constructs the relevance/dismissal questions;
the TypeSafe adapter only encodes the provider protocol. It preserves every
probability and actual model identity in a typed response, with usage in the
existing completion envelope. Memory code applies separate relevance/dismissal
policies (currently each uses the provisional 0.5 threshold). Ordinary LLMs
return selected question indices; normalization belongs to the memory owner.
Choice and Score can be added when a concrete caller needs them.

One Jet Offering contains the connection and encrypted key. Future operations
reuse that Offering, rather than creating scenario-specific Jet credentials.
Typed nonstream judgments can retain the canonical memory-retrieval,
introspection or verification-judge purpose. This enables new business callers
without modifying the provider adapter or mislabeling their purpose.

Current message/candidate truncation budgets are preserved. Provider errors,
timeouts or malformed answers retain the existing lexical relevance fallback
and no-dismissal fallback. The latter does not delete persistent memory: this
path removes injected session lessons only. No extra LLM retry is added.

TypeSafe streaming, tools, ordinary chat, extraction, reasoning and arbitrary
wire overrides are rejected before dispatch. Shared memory/reasoning defaults
exclude it, and chat default projection will not select it.

## Compare without changing deployment configuration

Use your existing Astra login and registered Offerings. Obtain the IDs with
`astra admin model list`, then run:

```sh
astra admin model compare <baseline-offering-id> <jet-offering-id>
```

This repeats the twelve built-in memory cases three times through the existing
authenticated Server completion endpoint and ledger. It creates a separate
comparison Session and never changes your active Session or deployment binding.
Backend order alternates by repetition. Reports go to a new private directory
under `/tmp`; the command prints its location and a readable summary.
Unavailable calls and malformed answers remain distinct from label errors.

For another replacement, provide a JSON case file:

```sh
astra admin model compare <baseline-offering-id> <jet-offering-id> \
  --cases examples/judgment-compare.json --repeat 3
```

Each case defines its operation, structured evidence/questions, optional expected
question IDs, and a probability threshold. All task criteria belong in the shared
questions/state. The LLM system message only specifies output formatting, so both
Offerings evaluate the same input. The example uses `verification_judge`; other
supported completion operations include `turn_intent` and `skill_auto_route`.
They are comparison entrypoints, not automatic enablement of Jet in those
production features.

`manifest.json` records the binary hash/version, normalized fixture hash, exact
Offerings, shared instructions and deadline. `results.jsonl` is flushed after each
call and retains raw probabilities, usage, actual typed-response model, input
hash, ordering coordinates, latency and failures. `summary.json` reports valid
and unavailable calls, label agreement and latency across all calls, including
failed and malformed responses. Private case contents stay
in the local report; users choose any custom evidence sent to their configured
Offerings. Costs are not guessed: the public catalog has no price fields, so the
runner reports actual token usage for comparison against Offering prices.

Thresholds can be compared against saved Jet probabilities without making new
paid calls. Repeated synthetic cases are not independent production coverage;
the concise-preference relevance labels are subjective. Compare task quality
and unhappy paths on a representative corpus before enabling a new replacement.

## See Jet usage in Explain

Enable `/explain on` before sending a request. The finished report separates
main-model tokens from auxiliary judgments, for example:

```text
Auxiliary tokens · Jet (jev1.13.0) · Memory judgment · in 420 · cache read 0 · cache write 0 · out 24 · 1/1 requests reported
```

This is an illustrative layout, not measured usage. TUI, text/HTML artifacts and
Web use the same physical-attempt facts. Missing provider counters show
`unknown`; partial reports remain labeled partial. Retries are separate physical
attempts, while replayed snapshots do not count them twice. A capture failure
shows `capture unavailable` and preserves the normal answer. This section does
not invent auxiliary timings or claim that Jet reduces main-agent rounds.

## Failure behavior

| Condition | Expected behavior |
| --- | --- |
| Empty Jet key | Cannot bind/use the Jet Offering; no provider dispatch |
| Unauthorized, rate limited, unavailable or timed out | Relevance uses the existing lexical fallback; no dismissal is inferred |
| Invalid answers, probabilities or question identities | Reject the judgment and use the same conservative business fallback |
| Valid answers with missing/invalid token metadata | Keep the judgment; missing counters remain unknown |
| Explain usage capture fails or exceeds its budget | Preserve the answer; report auxiliary capture unavailable |
| A comparison call fails or returns invalid answers | Keep every observation, label the failure and return an unsuccessful comparison |
| Comparison output directory already exists | Refuse overwrite and preserve the existing report |

These describe the contract and its deterministic regression coverage, not proof
that every live-provider failure has been reproduced. Optional auxiliary
inference failure does not become a primary task `ExecutionIncomplete` condition.

## Validate

Normal offline and online tests use local mock providers and fake keys. They do
not load `JET_KEY`, connect to Jet/DeepSeek, or run `admin model compare` against
real Offerings. Real-provider evaluation is an explicit harness action, separate
from these tests; the commands above perform that action and may incur charges.

```sh
cargo test -p astra-runtime --lib turn::llm::typesafe::tests
cargo test -p astra-runtime --lib memory_hooks::relevance::tests
cargo test -p astra-runtime --lib memory_catalog_query_tests
cargo test -p astra-cli --lib admin_cli::judgment_compare::tests
```

Only claim lower rounds if an existing call is eliminated: replacing one selector
LLM call with one Jet request reduces general-purpose LLM calls, not total model
requests or main-agent rounds. Existing selectors already batch all candidates.
