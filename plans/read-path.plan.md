# Read Path: Evolution and Doubt

Foundry records project history and epistemic state but never renders
them. This plan builds the first two read surfaces, both pure
projections over data the system already writes: `evolution` renders
the event log as a project timeline; `doubt` renders the live register
of unanswered questions and named assumptions. No new write machinery,
no new trust surface.

1. [x] Add an `evolution` CLI subcommand and dispatch it to a new `commands::evolution` module - files: crates/foundry-cli/src/main.rs, crates/foundry-cli/src/commands/mod.rs, new:crates/foundry-cli/src/commands/evolution.rs - run: cargo test -p foundry-cli - id: add-an-evolution-cli-subcommand
2. [x] Implement the event-timeline projection in `foundry-core`: read the recorded event log and group it into chronological project-history entries carrying event kinds, counts, and task, job, and review milestones - files: crates/foundry-core/src/event.rs, crates/foundry-core/src/graph.rs, new:crates/foundry-core/src/evolution.rs - run: cargo test -p foundry-core - id: implement-the-eventtimeline-projection-in
3. [x] Render the evolution timeline as plain text from the CLI, entries ordered oldest to newest with a per-kind summary - files: crates/foundry-cli/src/commands/evolution.rs, crates/foundry-core/src/lib.rs - run: cargo test -p foundry-cli - id: render-the-evolution-timeline-as
4. [ ] Add an integration test exercising the `evolution` command end-to-end against a seeded event log - files: crates/foundry-cli/src/commands/evolution.rs, new:crates/foundry-cli/tests/evolution.rs - run: cargo test -p foundry-cli - id: add-an-integration-test-exercising
5. [ ] Add a `doubt` CLI subcommand and dispatch it to a new `commands::doubt` module - files: crates/foundry-cli/src/main.rs, crates/foundry-cli/src/commands/mod.rs, new:crates/foundry-cli/src/commands/doubt.rs - run: cargo test -p foundry-cli - id: add-a-doubt-cli-subcommand
6. [ ] Implement the doubt-register projection in `foundry-core`: aggregate unanswered discourse questions (a question with no synthesis reply in its context) and the named assumptions stored in job-result governance envelopes - files: crates/foundry-core/src/discourse.rs, crates/foundry-core/src/graph.rs, new:crates/foundry-core/src/doubt.rs - run: cargo test -p foundry-core - id: implement-the-doubtregister-projection-in
7. [ ] Render the doubt register as plain text from the CLI, separating open questions from named assumptions, each with its provenance - files: crates/foundry-cli/src/commands/doubt.rs, crates/foundry-core/src/lib.rs - run: cargo test -p foundry-cli - id: render-the-doubt-register-as
8. [ ] Add an integration test exercising the `doubt` command end-to-end against seeded discourse turns and governed job results - files: crates/foundry-cli/src/commands/doubt.rs, new:crates/foundry-cli/tests/doubt.rs - run: cargo test -p foundry-cli - id: add-an-integration-test-exercising-2
