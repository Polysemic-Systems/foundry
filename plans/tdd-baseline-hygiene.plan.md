# TDD Baseline Hygiene

Dogfooding finding from the read-path stage: the persisted TDD baseline
is keyed by task key alone (`load_or_capture_tdd_baseline`) and the
attempt copy is reused as-is (`attempt::prepare`); both are retained on
failure. A baseline or attempt captured from a red workspace pins that
stale state to the task, so later runs re-execute the stale bytes as
"proof" even after the authoritative workspace has been repaired.
Recovery required undocumented manual deletion under `.foundry/`.

1. [ ] Discard the persisted TDD baseline and the attempt directory when the green-baseline proof fails because the workspace is red, so the next run re-captures from the repaired workspace; runner-infrastructure failures must keep both, since the workspace state is not at fault - files: crates/foundry-cli/src/commands/iterate.rs, crates/foundry-cli/src/attempt.rs - run: cargo test -p foundry-cli
2. [ ] The green-baseline failure message names the recovery action: it reports that the stale baseline and attempt were discarded and prints both paths - files: crates/foundry-cli/src/commands/iterate.rs - run: cargo test -p foundry-cli
