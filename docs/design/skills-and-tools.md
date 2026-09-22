# Skills and tools

> Status: target design contract.
> Last updated: 2026-07-19.

Skills and tools define model-facing capabilities. The capability system owns routing and admission; this document defines packaging and user/product semantics.

## Skills

A skill is a packaged capability with:

- instructions;
- examples;
- resources;
- optional tools or MCP bindings;
- input/output contract;
- permission requirements;
- evaluation cases;
- version metadata.

Skill maturity is progressive: prompt-only, structured prompt, tool-backed,
resource-backed, evaluated, then governed. A package must not claim a maturity
level unless its production discovery and activation path demonstrates it.

## Tools

A tool is a callable schema. Tool visibility and execution are decided by the capability system.

Resident tools expose a small stable `tools[]` contract. `tool_search` selects
deferred invocation contracts into canonical conversation evidence; it does not
inject their schemas into later `tools[]` requests. Invoke selected tools through
the resident `invoke_tool` carrier and reuse the selection across turns while
its contract and capability remain current. Selection is knowledge, not a
permission grant: request projection and execution still enforce current provider
and policy admission. After compaction removes necessary argument knowledge,
rediscovery is legitimate; do not require or prohibit it merely by turn count.

## Relationship

A skill may require tools, but it does not make those tools available by itself. Provider decision still controls whether a required capability can run in the current session.

## Skill lifecycle

```text
draft -> validated -> published -> activated -> deprecated -> archived
```

Activation may be scoped by user, workspace, agent, or policy.

The operating workflow is:

```text
author -> validate -> evaluate -> publish -> activate -> observe -> tune -> version
```

Governed skills declare their stable identity and version, required
capabilities, allowed providers, instructions/resources, input/output and
permission contracts, evaluation cases, compatibility, and rollback policy.

## Implementation constraints

- One shared owner parses manifests, resolves discovery paths, and validates
  packages. CLI, server, Web, and providers consume that owner instead of
  maintaining local loaders or compatibility-shaped copies.
- Before adding a registry, parser, provider, or lifecycle state, identify the
  current owner and production callers. A second implementation requires a real
  deployment or authority boundary, not convenience for one caller.
- A replacement migrates callers and removes the superseded implementation and
  its self-only tests in the same change. Temporary dual paths require an owner,
  expiry condition, and convergence test.
- Exported types and parser unit tests do not prove a usable skill. Tests must
  cover discovery, capability admission, activation, required tool/resource
  availability, failure diagnostics, and the user-visible outcome.
- Persistence changes require the real schema, query, transaction, migration,
  and rollback/failure path to be exercised against the supported database.

## Compatibility

Skill updates should declare:

- instruction-only change;
- schema-compatible change;
- schema-breaking change;
- provider requirement change;
- permission change.

Provider or permission changes require stronger review.

## Discovery

Skill discovery should be progressive:

- stable small index in prompt;
- deferred loading for full instructions/resources;
- capability-aware filtering;
- deterministic ordering;
- clear diagnostics for unavailable skill dependencies.

Catalog lifetime follows the execution boundary. The local CLI builds one
unified registry for the interactive session: filesystem skills are available
first, and the authenticated server catalog is joined once as an external
provider or after an explicit refresh. Edge executes the local filesystem
capability; it does not turn each tool call into a server catalog lookup. Web
and remote CLI runs use a user-scoped server registry retained by
the lifecycle service. Its database discovery is single-flight, cached for 60
seconds, and explicitly refreshable; the database provider also caches its
metadata snapshot and loaded skill bodies. A normal server turn therefore
reuses the catalog instead of walking `skills_registry` again.

## Observability

Track per skill version: invocation and success/failure counts, tool-call
validity, user correction rate, provider fallback/block rate, token cost, and
regression failures.

## Optional memory candidate judgment pilot

Memory relevance and explicit lesson dismissal use the existing selector boundary.
An optional server admin `judgment_offering_id` binds those judgments to a
registered Offering, including the TypeSafe System One adapter. Offering credentials
remain encrypted and server-owned. This binding never selects a memory extraction
model or changes agent/tool policy. See [the pilot guide](../guides/memory-judgment-pilot.md)
for configuration, fallback and validation limitations.

## Intent-first authoring journey

The normal user entrypoint accepts an outcome in natural language. The user
does not select a Harness, Session, Offering, verifier, snapshot, or provider
route. For example, these are complete requests:

* “帮我生成一个 Skill。”
* “帮我优化这个能力。”
* “把刚才反复做的流程变成可复用能力。”

The normal chat path uses the same model-facing capability mechanism as every
other tool:

```text
user message
  -> select the standard `skill_creator` tool
  -> apply capability projection, provider admission, and policy
  -> resolve the relevant conversation, current artifact, and evidence
  -> generate one candidate
  -> compare it with the appropriate baseline through the shared Evaluation
  -> return the candidate, verdict, evidence, cost, and limitations
```

The tool resolves `create` or `improve` only after a candidate exists and the
session-owned baseline can be identified. An exact active Skill identity is
strong evidence for improvement; without that evidence the tool creates a new
private candidate or reports that the operation is ambiguous. The generic
runtime may use the configured JEV or an explicitly selected LLM substitute
for eligible execution judgments, but authoring does not add a special
classifier. Judgment remains optional, cannot grant permission, and never
replaces the canonical tool admission boundary.

The first object is an owner-scoped instruction-only Skill, but the entrypoint
is intentionally named and modeled as authoring intent. Future adapters can
resolve a prompt, workflow, routing policy, or other capability without
teaching users a different control-plane vocabulary. Skillify generates a
candidate and its source evidence; the shared Evaluation owner creates trials,
settles the canonical Runs, and builds the report.

The user-visible result is small and concrete:

* the generated or improved candidate, with an immutable revision after publish;
* the evaluation outcome, including baseline/candidate behavior and whether
  the evidence is complete;
* links to the source evidence and the measured tool, context, provider,
  safety, reliability, and cost facts; and
* a clear next action such as use, publish, revise, or inspect evidence.

The UI may show progress such as “理解目标”, “生成候选”, and “真实评估”,
but it does not expose the implementation controls above. The existing
`/harnesses` and `/evaluations` surfaces remain operator and diagnostic views;
they are not the normal user journey. The standard tool and the Web/CLI
shortcuts call the same authoring operation. Authoring does not invent a task
criterion or verifier. When a canonical server-owned replay case is present,
the shared Evaluation is prepared and Web/CLI follow its frozen trial bindings
through the normal Run lifecycle and render the resulting report; otherwise
the result explicitly says that evaluation is unavailable.

The first adapter uses an explicit no-Skill baseline for creation and binds an
improvement comparison to the exact active Skill revision in the session. It
stores the generated candidate as a private draft and never activates it as a
side effect. When the context does not contain a replayable case and a
server-owned verifier, the result contains the candidate and an explicit
unavailable evaluation; that state never becomes a pass. A user can then
provide or record a suitable replay case before asking for the comparison
again.

## Evidence-backed evaluation and Skillify adapter

Evaluation is a shared capability for prompts, skills, routing, provider/model
bindings, memory policies, and workflows. The first executable slices are
deliberately bounded to private prompt comparisons and owner-scoped,
instruction-only Skill comparisons whose task outputs can be checked without
mutating an external system. Skillify is the first authoring adapter:
it turns selected work evidence into a candidate revision and citations, then
hands the candidate to the shared evaluation owner. It does not own trial
identity, assessment, or report semantics.

The Skillify adapter extends the existing personal-skill version store and
Skillify harness; it does not create a second skill registry, local adoption
authority, or execution loop.

Creation and optimization consume selected conversation evidence (Context,
Trace, and Journal facts). A source range, evidence watermark, redaction
policy, and the resulting candidate content are recorded together. A draft may
be useful before it is proven, but it must say when it is based on one source,
has missing facts, or contains unresolved user corrections. Tool frequency is
observational evidence only and never grants a capability or permission.

An evaluated revision is immutable. Its experiment specification pins the
baseline and candidate content hashes, frozen cases, verifier versions, model
and provider bindings, other skills, isolation profile, repetition/order plan,
and bounded budget. Each case/arm/repetition has a stable trial identity;
retries are separate attempts and retain their own cost and uncertainty. The
no-skill arm keeps normal base capabilities, and the executor cannot read the
authoring conversation, the other arm's output, hidden answers, or production
learning state. Unsupported tool-backed or external-side-effect tasks fail
preflight instead of receiving a misleading score. The current Skill execution
adapter pins one published owner-scoped revision, recomputes its
manifest-plus-Markdown identity, and rejects tool-backed, forked, remote,
hook, or other external-side-effect surfaces at preflight.

Reports are derived from persisted trial facts and expose content, behavior,
and result differences with links to authorized evidence. Measured,
verified, assessed, inferred, and unavailable facts remain distinct. Missing
usage or incomplete traces are reported as missing; they are never converted
to zero. A report can be regenerated without rerunning a trial.

Private adoption is an explicit compare-and-set against the currently active
revision. The activation request carries `expected_active_version_id`; `null`
means that no revision is expected. The server locks only the target session,
checks the revision content hash before writing, and returns a conflict when
the expectation is stale. Repeating the same target is idempotent only when
the request names the currently active revision as its expectation. A conflict
preserves both revisions and is visible to the user. Running trials keep their
pinned revision. Rollback is another recorded adoption, and a follow-up real
invocation must prove that the adopted revision is loaded with its version/hash
identity intact.

Authoring requests may carry an `idempotency_key` to replay one attempt. Each new
user submission uses a new key; omitting it creates a fresh attempt. Replaying a
running or failed attempt returns a conflict, rather than claiming a candidate is
ready. The authoring result links directly to the persisted candidate's existing
review and publication flow. Publication remains an explicit user action.

Database catalog discovery preserves user-owned precedence, then creation order,
and caches the exact selected record ID. Loading uses that ID so a newer public
version cannot replace a private override. This adds no per-turn database reads.
