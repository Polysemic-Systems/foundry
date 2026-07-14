# ADR 0001: Living-systems conformance

- Status: Accepted
- Date: 2026-07-14
- Scope: persisted derived knowledge and execution evidence

## Context

Foundry is intended to turn intent into plans, changes, evidence, review, and
deployment. A single unqualified graph would make inferred links look like
facts, overwrite disagreement, and retain derived material indefinitely.

We organize the system around three axes:

| Axis | Triad | Function |
| --- | --- | --- |
| Epistemic | Polysemic Antifragility · Organic Ontology · Active Forgetting | How the system knows |
| Temporal | Traced Lineage · Negotiated Boundaries · Continuous Revision | How the system changes |
| Governance | Named Assumptions · Auditable Transformations · Scheduled Forgetting | How the system stays accountable |

## Intuitive reading

1. Preserve conflicting claims until a named policy selects an operational
   answer. A decision does not erase the disagreement.
2. Label knowledge as observed, inferred, legislated, or historical.
3. Trace derived outputs to sources and transformations.
4. Name material assumptions instead of hiding them in prompts or code.
5. Give retained derived material an explicit deletion, review, or preservation
   policy before persistence.
6. Revise by supersession and lineage, not silent overwrite.

## Counterintuitive review

- Ambiguity is not always valuable. Commands, resource limits, cryptographic
  digests, and safety gates require one operational interpretation. Alternatives
  may be retained as claims, but execution crosses a negotiated boundary using
  a legislated value.
- Forgetting is not indiscriminate deletion. Source code, approvals, financial
  records, and incident evidence can have incompatible obligations. Preservation
  is allowed only with an explicit basis and owner; it is not the default.
- Provenance does not imply truth. A perfectly traced false claim remains false.
- More lineage can reduce privacy and comprehensibility. Store references and
  digests where possible, not needless copies of source content.
- An erasure receipt must not recreate the erased material through names,
  excerpts, prompts, or reversible identifiers.
- Continuous revision must not mutate historical evidence. Corrections append a
  superseding record and preserve the prior decision only as policy permits.
- Generated ontologies can reproduce corpus power structures. Legislation may
  overrule an inference, but the authority and assumption behind that override
  must be visible.

## Decision

Foundry will enforce a `GovernanceEnvelope` on persisted derived evidence. It
contains a knowledge layer, source references, named assumptions, an auditable
transformation, an owner, and an explicit retention policy.

The executable contract initially applies to job results and artifacts. It does
not yet claim universal coverage. Graph claims, model answers, reviews, and
deployments must adopt the same contract before they become authoritative
stores.

Retention has three explicit forms:

- delete after a deadline;
- review at a deadline;
- preserve for a named basis.

The contract distinguishes structural conformance from temporal disposition. A
record can remain structurally valid while becoming due for review or deletion.

## Consequences

- The runner cannot persist evidence without provenance and lifecycle policy.
- Operational boundaries remain deterministic even when upstream meaning is
  contested.
- Future deletion must cascade to embeddings, indexes, artifacts, snapshots, and
  other derivations, leaving only a content-minimized receipt.
- The conformance contract must grow through reviewed revisions; this ADR is not
  a timeless ontology.

## Non-goals

- Defining a universal ontology.
- Treating confidence as truth.
- Automatically resolving contradictions.
- Implementing cascading erasure in this decision alone.
