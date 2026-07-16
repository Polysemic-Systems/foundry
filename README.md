# Foundry

A local-first, self-building production system.

> Code is the spec. The system builds itself.

Foundry consumes the MIT-licensed `digest`, `lethe`, and `polysemic-core`
crates from the sibling [polysemic](../polysemic/) workspace as path
dependencies: building Foundry requires the `polysemic/` checkout next to
`foundry/`. Digest is Foundry's model-output boundary (named repairs,
ambiguity as questions); Lethe is its evidence-erasure contract.

## Quick start

```bash
just build      # Build the system
just init       # Create .foundry/ database
just index      # Index this codebase into itself
just plan       # Show the bootstrap plan
just check      # Run all checks
```

## Safe iteration

`foundry iterate` takes the next runnable plan task, executes its `run` command
in a bounded, network-disabled Podman container, and persists the command,
output, changed files, test result, artifacts, and retention metadata. The plan
does not advance until the captured job is explicitly reviewed:

```bash
just iterate
cargo run -p foundry-cli -- review-approve \
  --root . \
  --task 'plans/bootstrap.plan.md#task-1' --job <job-uuid> \
  --reviewer <identity> --reason '<evidence-based decision>'
just iterate  # reflects the approval in the plan and selects the next task
```

Use `review-reject` with the same arguments to return the task to `ready` for a
new attempt. Reusing an idempotency key returns the original immutable result.

### The digest boundary

Every model-generated payload crosses the `digest` seam before Foundry trusts
its shape. `foundry propose` asks the model for one JSON object
(`{"spec": ..., "tasks": [...]}`); malformed output is repaired with every fix
named on a printed ledger (stripped fences, requoted strings, Python literals,
trailing commas, case-folded enums), and genuine ambiguity becomes questions
the human can answer interactively instead of a silent guess. Review-draft
agents must likewise emit `{"recommendation": "approve" | "reject", "body":
"<Socratic markdown>"}`; the recommendation is schema-enforced and
case-folded. Each crossing is recorded as a `model_output_digested` event with
its repair, question, and answer ledgers.

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

### Evidence-grounded review TUI

For successful jobs, Foundry prepares two structurally independent advisory
reviews: a deterministic evidence-policy audit and a model-generated adversarial
critique. The drafts are immutable graph records and cannot transition task
state. Only the final edited and attributable human resolution has authority:

```bash
export FOUNDRY_REVIEW_AGENT_COMMAND='kimi --prompt'
just review-tui 'plans/features.plan.md#task-4' <job-uuid> megloff1
```

If that job already has a human review, the same command opens a retrospective
session. Foundry keeps the historical decision and task state fixed while
storing the selected/edited rationale as learning context for later agents.

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
export FOUNDRY_AGENT_COMMAND='codex exec --full-auto -'
export FOUNDRY_AGENT_NETWORK=on # explicit consent for a remote coding agent
just iterate-tdd
```

Agents that require the prompt as a command argument are also supported when
the configured command ends in `--prompt` or `-p`, for example
`FOUNDRY_AGENT_COMMAND='kimi --prompt'`.

Foundry copies the authoritative workspace into `.foundry/attempts/` and gives
only that copy write access through Bubblewrap. The sandbox uses an empty root
with only system runtime files, the attempt, a clean temporary home, and an
allowlist of the selected agent's authentication/configuration files mounted;
the user's home and prior agent sessions are not visible. Network access is
disabled unless `FOUNDRY_AGENT_NETWORK=on` is explicitly set. It first proves
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
it; using that setting weakens host-write isolation.

For file-backed graphs, change bytes are stored once in
`.foundry/blobs/sha256/`; SQLite stores immutable digest references rather than
large byte arrays. Reads hydrate and verify those objects and fail closed when
an object is missing or corrupt. Legacy inline evidence remains readable.

Rejected reviews are active inputs to the next TDD attempt: Foundry loads the
latest rejection reason and its job evidence from the graph and asks the test
phase to reproduce the rejected behavior. If durable verification fails because
of compilation or tests, the next iteration invokes a repair phase with that
output. Failures classified as runner infrastructure are retried without asking
the editor agent to change application code.

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
