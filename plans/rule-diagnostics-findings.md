# Rule-based diagnostics findings

## Verified state

- `cargo test --workspace` passes (5 graph tests).
- `just check` passes (fmt + clippy + tests).
- `just check-rules` runs and reports:
  - `[PASS] Edge integrity`
  - `[WARN] Code files are linked` — all indexed code files are currently unlinked.
  - `[PASS] No empty payloads`
- Ollama is installed at `/usr/sbin/ollama`, ready for the next layer.

## Findings (ranked by impact)

### 1. Edge-integrity rule is defensive dead code under normal use (high)

`Graph::create_edge` is protected by SQLite foreign keys, so the public API cannot
insert an edge that references a missing node. The rule is correct, but it is not
exercised by any test or normal path. Without evidence that it fires, we cannot
trust it as part of a self-healing graph.

Root cause: the graph enforces referential integrity at the storage layer, which
is good, but the diagnostic layer needs a corruption/repair test path.

Fix: add a test-only helper that leaves a dangling edge, then assert the rule
fails. Keep the FK enforcement; do not weaken it.

### 2. Code nodes are ingested but not linked to intent (high)

`just index` creates `Code` nodes with path/line payloads, but no edges connect
them to tasks, plans, tests, or rules. The warning from `CodeLinkedRule` is an
honest reflection of state, not a false positive. Until the planner/linker knows
how to map code to intent, RAG over the graph will miss semantic context.

Root cause: indexing is a separate step from planning/linking; the system ingests
before it relates.

Fix: keep the warning. The next meaningful move is to parse `plans/` into `Plan`
and `Task` nodes and create `implements`/`depends_on` edges from tasks to code.
Synthetic auto-links would only silence the rule.

### 3. Empty-payload semantics are implicit (medium)

`NoEmptyPayloadRule` treats `serde_json::Value::Object(Default::default())` as
empty. This matches `Node::new(..., serde_json::json!({}))`. Later, intentional
stubs may need an explicit flag; for now the rule is predictable.

Root cause: no schema-level distinction between "empty payload" and "stub".

Fix: none yet; revisit when a domain actually needs stubs.

### 4. Rules are not yet part of the graph (medium)

Rules are Rust code and runtime objects. They are not represented as `Rule` nodes
with edges to the nodes they govern. The `RuleTriggered` event exists, but
`check-rules` does not emit it.

Root cause: the rule engine was bootstrapped as code first.

Fix: after rule tests are solid, represent each rule as a node and emit
`RuleTriggered` events with results.

### 5. Idempotency and recovery are incomplete (medium)

- Migrations are versioned but not hashed.
- There is no replay-from-events path.
- There is no corruption scanner beyond the three rules.

Root cause: the system is young; resilience was deferred.

Fix: add schema-version assertion, event replay tests, and a scheduled full-scan
rule runner before claiming self-healing.

## Open loops to close before expanding

1. Prove each rule catches a real violation in a unit test.
2. Decide whether `CodeLinkedRule` stays a warning or becomes a fail once linking
   is implemented.
3. Emit `RuleTriggered` events from `check-rules` so the graph learns its own
   health history.
