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
