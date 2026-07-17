# Foundry

A local-first, self-building production system.

> Code is the spec. The system builds itself.

Foundry consumes the MIT-licensed `digest`, `lethe`, and `polysemic-core`
crates from the sibling [polysemic](../polysemic/) workspace as path
dependencies: building Foundry requires the `polysemic/` checkout next to
`foundry/`. Digest is Foundry's model-output boundary (named repairs,
ambiguity as questions); Lethe is its evidence-erasure contract.

## Status: what is real and what is not yet

<!-- BEGIN GENERATED STATUS -->
Naming assumptions is the house rule, so it applies to this README too.

**Working and tested end-to-end:** the plan → sandboxed job → immutable
evidence → human review → journaled promotion vertical slice; the
test-first editor-agent loop with red/green policy enforcement; the
two-draft Socratic review questionnaire with separate advisory choice,
decision confirmation, human rationale, and custom questions; checksum-verified migrations with a
doctor-side audit; the digest model boundary (repair ledgers, ambiguity
as answerable questions, recorded events); the lethe retention sweep
(governed deletion, shared-blob reference counting, erasure receipts);
and a repository lease used by `iterate`, enforcing sweeps, and
`reconcile-plan --apply`, as well as index/rebuild/heal, review resolution,
promotion, and snapshot create/restore.

**Present but not yet what the vocabulary promises:** events are an
append-only audit log, not a communication bus — there is no dispatcher
or replay-from-events path, and legacy/non-task event payloads can still
refer to missing graph node IDs (current plan/task indexing preserves
identity and task events also carry durable keys); plan task dependencies
are not parsed, so plans execute as ordered lists rather than DAGs;
semantic search is a full-table scan over JSON-encoded embeddings
(sqlite-vec is the plan); doctor reports orphaned blobs only after its
one-hour sweep age guard; and test coverage remains thinnest around
evidence-store hardening and Podman runner timeout/cancel paths.

This is a working skeleton with unusually strict gates, not a product.
The seams where you would extend it are marked in doc comments.
<!-- END GENERATED STATUS -->

This block is sourced from [`docs/status.md`](docs/status.md). Run
`scripts/sync-readme-status.sh` after editing it; the repository gates reject a
stale copy.

## Development gates

The same versioned commands run locally and in CI:

```bash
just gate-fast       # generated docs, architecture/source policy, fmt, clippy
just gate-full       # gate-fast plus the complete workspace test suite
just install-hooks   # opt this checkout into the versioned Git hooks
```

`.githooks/pre-commit` runs the fast gate and `.githooks/pre-push` runs the
full gate. Hooks are an early-feedback layer and can be bypassed, so CI invokes
`scripts/gate-full.sh` directly as the authority. The fast gate separately
denies production `unwrap()` calls, file-derived plans parsed with hardcoded
titles, string-split structured manifests, stale generated status, and module
growth beyond the ratchets in
[`docs/adr/0001-architecture-budgets.md`](docs/adr/0001-architecture-budgets.md).

## Quick start

Foundry currently requires the pinned Rust toolchain, Podman, and the sibling
`polysemic/` checkout described above. `just index` also requires a running
Ollama service because the recipe generates embeddings.

```bash
just build      # Build the system
just init       # Create .foundry/ database
just index      # Index this codebase into itself
just plan       # Show the bootstrap plan
just check      # Run all checks
just doctor     # Audit the local graph and evidence store
just reconcile  # Report filesystem/graph drift
```

`just sandbox-build` builds the workspace inside an interactive Podman
container; it does not install, publish, or deploy Foundry. `just deploy`
remains as a backwards-compatible alias.

Use `foundry lease` to inspect the repository mutation lease. Use
`foundry reconcile-plan` to audit stable task identities and
`foundry reconcile-plan --apply` to repair safe identity drift; the apply form
takes the repository lease.

Operational tracing is written to stderr so command stdout remains stable.
`FOUNDRY_LOG` accepts tracing filter directives, and
`FOUNDRY_LOG_FORMAT=json` emits machine-readable events carrying task, job,
review-resolution, promotion, and lease correlation fields:

```bash
FOUNDRY_LOG=info FOUNDRY_LOG_FORMAT=json foundry lease
```

## Safe iteration

`foundry iterate` takes the next runnable plan task, executes its `run` command
in a bounded, network-disabled Podman container, and persists the command,
output, changed files, test result, artifacts, and retention metadata. The plan
does not advance until the captured job is explicitly reviewed:

The checked-in feature backlog is complete. Add a live task first with
`just propose '<feature>'` (or edit `plans/features.plan.md` directly) when
following this workflow in a fresh checkout.

```bash
just iterate
# Copy the task key and job UUID printed by iterate. The task must be in Review.
just review-approve 'plans/features.plan.md#<stable-task-id>' \
  '<job-uuid>' '<reviewer-identity>' '<evidence-based decision>'
just iterate  # reflects the approval in the plan and selects the next task
```

Use `just review-reject` with the same four arguments to return the task to
`ready` for a new attempt. Reusing an idempotency key returns the original
immutable result.

### The digest boundary

Every model-generated payload crosses the `digest` seam before Foundry trusts
its shape. `foundry propose` is interactive: it asks the model for
decision-bearing questions, reads multiline human answers, generates one JSON
object (`{"spec": ..., "tasks": [...]}`), and asks the human to approve, edit,
or abort before changing the plan. Run it with `just propose '<feature>'`; it
requires a running Ollama service. Malformed output is repaired with every fix
named on a printed ledger (stripped fences, requoted strings, Python literals,
trailing commas, case-folded enums), and genuine ambiguity becomes questions
the human can answer instead of a silent guess. Review-draft agents must
likewise emit `{"recommendation": "approve" | "reject", "body": "<Socratic
markdown>"}`; the recommendation is schema-enforced and case-folded. Each
crossing is recorded as a `model_output_digested` event with its repair,
question, and answer ledgers.

### Retention sweep

Every job result carries a governed `RetentionPolicy`; `foundry sweep`
enforces it. The default policy stays `ReviewAfter { now + 30 days }`; pass
`--evidence-retention-days N` to `job-run` (or set
`FOUNDRY_EVIDENCE_RETENTION_DAYS`) to opt a job's evidence into
`DeleteAfter { now + N days }`.

```bash
foundry sweep            # dry-run report: delete-due, review-due, deferred, retained, quarantined
foundry sweep --enforce  # erase delete-due evidence, collect orphaned blobs, record receipts
```

Enforcement runs through lethe's erasure contract: two stores (the SQLite
`job_results` row and the content-addressed blob store) are erased and
independently verified absent by an `ErasureCoordinator`; blobs shared with
surviving results are kept by reference counting. Receipts are one-way
commitments (`lethe://request/<hash>`, `foundry://erasure/<store>/sha256:<hash>`)
recorded as `evidence_erased` and `retention_swept` events — counts and hashes,
never content. The `jobs`, `reviews`, and `events` rows are append-only history
and are never deleted. Delete-due evidence whose task sits in review is
deferred: retention never removes evidence from under an open human decision.
Malformed stored results are quarantined and reported, and their blobs are
conservatively treated as referenced. Do not run `sweep --enforce` while jobs
are actively executing (a one-hour age guard protects freshly written blobs).

### Snapshot safety

Snapshot creation checkpoints the SQLite WAL while holding the repository
mutation lease. Restore accepts one non-empty snapshot name, validates an
immutable read-only source against `integrity_check` and Foundry migrations in
memory, then restores through SQLite's online-backup transaction under the
same lease. A corrupt or incompatible snapshot is rejected before the current
database is touched. Restore rewinds retention history, so rerun
`foundry sweep --enforce` afterward.

### Evidence-grounded review TUI

For successful jobs, Foundry prepares two structurally independent advisory
reviews: a deterministic evidence-policy audit and a model-generated adversarial
critique. The drafts are immutable graph records and cannot transition task
state. The terminal then runs an active questionnaire:

- choose the evidence review, adversarial review, both, or neither;
- explicitly confirm approve or reject;
- answer the decision-bearing challenge in your own words; and
- optionally add custom questions or ideas.

Choosing a draft never copies its prose or recommendation into the human answer.
Signing remains disabled until the required answers are present, and verbatim
advisory prose is refused as a rationale. Only the resulting attributable human
resolution has authority:

```bash
export FOUNDRY_REVIEW_AGENT_COMMAND='kimi --prompt'
# Use the task key and job UUID printed by a successful iterate run.
just review-tui 'plans/features.plan.md#<stable-task-id>' '<job-uuid>' '<reviewer-identity>'
```

For an initial decision, the task must be awaiting review and the job must
belong to it. If that job already has a human review, the same command opens a
retrospective session. Foundry keeps the historical decision and task state
fixed while requiring a fresh advisory choice and rationale, then stores that
answer as learning context for later agents.

Staged jobs retain SHA-256-addressed before/after bytes for every changed file,
the aggregate patch digest, executor image identity, command output, and test
results. Review prompts receive bounded content previews; the complete bytes
remain in the durable `JobResult`. Sandbox jobs print a compact verification
summary by default. Direct
`job-run` callers that need the complete machine-readable record can pass
`--json`; the full stdout and stderr are always retained as durable evidence in
SQLite.

### Socratic discourse

Every decision-bearing model interaction follows one discourse contract: begin
with a shared question, separate observations from assumptions, surface a
competing interpretation, identify falsifying evidence, and leave authority
with the human. Questions must be capable of changing a decision or next
action; routine status output remains direct.

Foundry persists these interactions as typed, reply-linked `discourse_turns`
with accountable speakers and epistemic acts (`question`, `observation`,
`assumption`, `challenge`, and `synthesis`). Review drafts become evidence and
adversarial partner turns; the edited human resolution becomes the final
synthesis retained for future learning.

The interface shows both drafts side by side. Use `1` or `2` as an editable
starting point, `0` to write an independent review, `e` to edit, `a`/`r` to set
the decision, and `s` to sign and submit. Foundry stores both originals, the
selected draft, the final text, edit similarity, reviewer identity, and the
decision. Later review prompts receive recent human resolutions as learning
context; rejected resolutions also become acceptance constraints for the next
TDD attempt. Generated drafts remain advisory regardless of their recommendation.

### Test-driven iteration

Foundry can delegate workspace edits to an external agent while retaining its
own sandboxed verification and review gate. Configure an agent that accepts its
instructions on standard input, then run the feature plan:

```bash
export FOUNDRY_AGENT_COMMAND='codex exec --sandbox workspace-write --skip-git-repo-check -'
# --skip-git-repo-check: attempt copies deliberately exclude .git, and codex
# otherwise refuses to run outside a trusted (git) directory.
export FOUNDRY_AGENT_NETWORK=on # explicit consent for a remote coding agent
just iterate-tdd
```

Agents that require the prompt as a command argument are also supported when
the configured command ends in `--prompt` or `-p`, for example
`FOUNDRY_AGENT_COMMAND='kimi --prompt'`.

Foundry copies the authoritative workspace into `.foundry/attempts/` and gives
only that copy write access through Bubblewrap. The sandbox uses an empty root
with only system runtime files, the attempt, and a private temporary home.
Foundry copies a narrow allowlist of the selected agent's authentication and
configuration files into that home, then removes it on every return path; the
rest of the user's home and prior agent sessions are not visible. Network
access is disabled unless `FOUNDRY_AGENT_NETWORK=on` is explicitly set.
A network-enabled agent can read and transmit its copied authentication
material and can reach services in the host network namespace, so enable it
only for an agent you trust with those credentials. It first proves
the task command is green. The red phase must add
or modify only dedicated tests (or an existing Rust `#[cfg(test)]` section), and
the command must then fail. The green phase cannot change or remove the test
that established the red failure. Foundry then verifies the implementation in
network-disabled Podman and records the staged patch as durable evidence.
Rejection discards the staged outcome. Approval conflict-checks and promotes the
recorded bytes into the authoritative workspace through an exclusive,
crash-recoverable promotion journal. Tasks without a `- run:` tag use
`cargo test` in TDD mode.

Bubblewrap is required by default. `FOUNDRY_AGENT_SANDBOX=off` is an explicit,
warning-producing compatibility escape hatch for systems that cannot provide
it; the agent then inherits the host environment, HOME, process and network
namespaces, and unconstrained filesystem access. Remote agents still require
the separate `FOUNDRY_AGENT_NETWORK=on` consent.

For file-backed graphs, change bytes are stored once in
`.foundry/blobs/sha256/`; SQLite stores immutable digest references rather than
large byte arrays. Reads hydrate and verify those objects and fail closed when
an object is missing or corrupt. Foundry keeps `.foundry/` private to the
current user (`0700`, with sensitive files at `0600`) and migrates older local
state permissions when taking the repository lease. Legacy inline evidence
remains readable.

Rejected reviews are active inputs to the next TDD attempt: Foundry loads the
latest rejection reason and its job evidence from the graph and asks the test
phase to reproduce the rejected behavior. If durable verification fails because
of compilation or tests, the next iteration invokes a repair phase with that
output. A repair-only retry is explicitly recorded as lacking a revalidated
red phase, and the deterministic review treats that as a blocker rather than
inheriting authority from an earlier process. Failures classified as runner
infrastructure are retried without asking the editor agent to change
application code.

Use `just iterate-tdd --debug-runner` to print the effective Podman image,
network mode, resource limits, and environment, then run a non-durable
`rustc --version` preflight before agents or durable verification jobs start.

Plan file hints are executable contracts. Referenced files must exist; an
intentional creation must use `new:path`. Run commands must pass Foundry's safe
command parser before a proposal can be appended or a task can execute.

## Design

- **One graph**: every artifact is a node (task, code, test, review, deploy, feedback, rule, model, env, plan).
- **Domain languages**: each subsystem owns its vocabulary and types.
- **Hybrid intelligence**: deterministic rules first, local/specialized models second, frontier models last.
- **Internal RAG**: code, plans, failures, and rules are embedded and retrievable locally.
- **Code as spec**: Rust types, schemas, tests, and `just` recipes are the documentation.

## Stack

- Rust 1.92
- SQLite + FTS5 + sqlite-vec (planned)
- Ollama for local models
- Podman/QEMU for runners

## License

GPL-3.0-or-later
