use anyhow::{Context, Result, bail};
use foundry_core::{
    Event, Graph, KeyMigrationRecord, NodeKind, Plan, RuleResult, TaskState, plan_reconcile,
};
use std::fs;
use std::path::Path;
use std::process::Command;
use walkdir::WalkDir;

use crate::commands::common::resolve_plan_path;
use crate::{indexer, lease, sweep};

pub fn cmd_init(root: &Path) -> Result<()> {
    let foundry_dir = root.join(".foundry");
    fs::create_dir_all(&foundry_dir).with_context(|| format!("creating {:?}", foundry_dir))?;
    crate::lease::harden_repository_state(root)?;

    let db_path = foundry_dir.join("db.sqlite");
    Graph::open(&db_path).with_context(|| format!("opening graph at {:?}", db_path))?;

    println!(
        "Initialized foundry at {:?}",
        root.canonicalize().unwrap_or_else(|_| root.to_path_buf())
    );
    println!("Database: {:?}", db_path);
    Ok(())
}

pub fn cmd_plan() -> Result<()> {
    let plan_path = Path::new("./plans/bootstrap.plan.md");
    let plan_text = fs::read_to_string(plan_path)
        .with_context(|| format!("reading plan at {:?}", plan_path))?;
    let plan = Plan::parse_path(plan_path, &plan_text);
    print!("{}", plan);
    Ok(())
}

pub fn cmd_list(db: &Path, kind: Option<&str>) -> Result<()> {
    let graph = Graph::open(db).with_context(|| format!("opening graph at {:?}", db))?;

    let kind_filter = kind.and_then(|k| k.parse::<NodeKind>().ok());

    let nodes = graph.list_nodes(kind_filter).context("listing nodes")?;
    for node in nodes {
        println!("{}\t{}\t{}", node.id.0, node.kind.as_str(), node.name);
    }
    Ok(())
}

pub fn cmd_search(db: &Path, query: &str) -> Result<()> {
    let graph = Graph::open(db).with_context(|| format!("opening graph at {:?}", db))?;
    let safe = foundry_core::sanitize_query(query);
    let results = graph.search_code(&safe).context("searching code")?;
    for (node, _content) in results {
        println!("{}\t{}", node.id.0, node.name);
    }
    Ok(())
}

pub fn cmd_reconcile(root: &Path, db: &Path) -> Result<()> {
    let (in_sync, missing_on_disk, not_in_graph) = reconcile_state(root, db)?;

    if in_sync {
        println!("Graph and filesystem are in sync.");
        return Ok(());
    }

    if !missing_on_disk.is_empty() {
        println!("In graph but missing on disk ({}):", missing_on_disk.len());
        for path in missing_on_disk {
            println!("  - {}", path);
        }
    }
    if !not_in_graph.is_empty() {
        println!("On disk but not in graph ({}):", not_in_graph.len());
        for path in not_in_graph {
            println!("  + {}", path);
        }
    }

    Ok(())
}

pub fn cmd_reconcile_plan(plan_path: &Path, root: &Path, db: &Path, apply: bool) -> Result<()> {
    // Apply owns one coherent lease-protected observation and mutation. A
    // dry run remains read-only and intentionally takes no lease.
    let mutation = if apply {
        let mutation =
            lease::acquire_repository(root, &lease::default_owner(), "reconcile-plan --apply")
                .map_err(|refusal| anyhow::anyhow!("{refusal}"))?;
        Some(mutation)
    } else {
        None
    };

    let (plan_path, plan_relative) = resolve_plan_path(root, plan_path)?;
    if let Some(mutation) = &mutation {
        mutation.require_path(&plan_path)?;
    }
    let mut graph = Graph::open(db).with_context(|| format!("opening graph at {:?}", db))?;
    let plan_text = fs::read_to_string(&plan_path)
        .with_context(|| format!("reading plan at {:?}", plan_path))?;
    let (mut plan, invalid_ids) = Plan::parse_path_strict(&plan_path, &plan_text);
    let states = graph.task_keys_with_state(&format!("{plan_relative}#"))?;
    let report = plan_reconcile::reconcile(&plan_relative, &plan, &invalid_ids, &states);

    if report.is_clean() {
        println!("Plan and graph agree: every task key is stable and both directions are in sync.");
        return Ok(());
    }

    for invalid in &report.invalid_ids {
        println!(
            "INVALID ID    line {}: {} (task is invisible to iteration until fixed)",
            invalid.line_index + 1,
            invalid.error
        );
    }
    for id in &report.derived_ids {
        println!("DERIVED ID    {id}: no explicit ` - id:` tag in the plan file");
    }
    let state_label =
        |state: &Option<TaskState>| state.map(TaskState::as_str).unwrap_or("history only");
    for migration in &report.migratable {
        println!(
            "LEGACY KEY    {} -> {} ({}, {})",
            migration.old_key,
            migration.new_key,
            migration.description,
            state_label(&migration.state)
        );
    }
    for (key, state) in &report.orphaned {
        println!(
            "ORPHANED      {key} ({}): matches no current task; inspect before deciding",
            state_label(state)
        );
    }
    for id in &report.unmarked_done {
        println!("UNMARKED DONE {id}: graph says Done, plan says [ ]");
    }
    for (id, state) in &report.marked_done_but_graph_disagrees {
        println!(
            "PLAN AHEAD    {id}: plan says [x], graph says {}",
            state.as_str()
        );
    }

    if !apply {
        println!(
            "\nDry run. `--apply` migrates legacy keys, persists id tags, and syncs done marks."
        );
        return Ok(());
    }
    if !report.invalid_ids.is_empty() {
        bail!("fix invalid ids in the plan file before applying repairs");
    }

    apply_plan_reconciliation(
        mutation
            .as_ref()
            .context("apply requires a mutation lease")?,
        &mut graph,
        &plan_path,
        &plan_relative,
        &mut plan,
        &report,
    )
}

fn apply_plan_reconciliation(
    mutation: &lease::RepositoryMutation,
    graph: &mut Graph,
    plan_path: &Path,
    plan_relative: &str,
    plan: &mut Plan,
    report: &foundry_core::plan_reconcile::PlanReconcileReport,
) -> Result<()> {
    mutation.require_path(plan_path)?;
    for migration in &report.migratable {
        graph
            .migrate_task_key(&migration.old_key, &migration.new_key)
            .with_context(|| format!("migrating {}", migration.old_key))?;
    }
    let persisted = plan.persist_derived_ids();
    for id in &report.unmarked_done {
        plan.mark_done(id);
    }
    if !persisted.is_empty() || !report.unmarked_done.is_empty() {
        fs::write(plan_path, plan.to_string())
            .with_context(|| format!("writing plan to {:?}", plan_path))?;
    }
    graph
        .index_plan(plan_relative, plan)
        .context("reindexing plan after reconciliation")?;
    graph
        .record_event(&Event::PlanReconciled {
            plan_path: plan_relative.to_string(),
            migrated_keys: report
                .migratable
                .iter()
                .map(|m| KeyMigrationRecord {
                    old_key: m.old_key.clone(),
                    new_key: m.new_key.clone(),
                })
                .collect(),
            persisted_ids: persisted.iter().map(|id| id.to_string()).collect(),
        })
        .context("recording plan reconciliation event")?;

    println!(
        "\nApplied: {} key migration(s), {} id tag(s) persisted, {} done mark(s) synced.",
        report.migratable.len(),
        persisted.len(),
        report.unmarked_done.len()
    );
    if !report.orphaned.is_empty() {
        println!(
            "Left alone: {} orphaned key(s) — deletion is a human decision.",
            report.orphaned.len()
        );
    }
    Ok(())
}

fn reconcile_state(root: &Path, db: &Path) -> Result<(bool, Vec<String>, Vec<String>)> {
    let graph = Graph::open(db).with_context(|| format!("opening graph at {:?}", db))?;

    let code_nodes = graph
        .list_nodes(Some(NodeKind::Code))
        .context("listing code nodes")?;
    let in_graph: std::collections::HashSet<String> =
        code_nodes.into_iter().map(|n| n.name).collect();

    let mut on_disk = std::collections::HashSet::new();
    for entry in WalkDir::new(root)
        .into_iter()
        .filter_entry(|e| !is_ignored(e.path()))
    {
        let entry = entry?;
        if !entry.file_type().is_file() {
            continue;
        }
        if fs::read_to_string(entry.path()).is_err() {
            continue;
        }
        let relative = entry
            .path()
            .strip_prefix(root)
            .unwrap_or(entry.path())
            .to_string_lossy()
            .to_string();
        // Plan files are represented as Plan nodes, not Code nodes.
        if relative.ends_with(".plan.md") {
            continue;
        }
        on_disk.insert(relative);
    }

    let missing_on_disk: Vec<String> = in_graph.difference(&on_disk).cloned().collect();
    let not_in_graph: Vec<String> = on_disk.difference(&in_graph).cloned().collect();
    let in_sync = missing_on_disk.is_empty() && not_in_graph.is_empty();

    Ok((in_sync, missing_on_disk, not_in_graph))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Health {
    Ok,
    Warn,
    Fail,
}

struct Check {
    name: String,
    health: Health,
    message: String,
}

fn ok(name: impl Into<String>, message: impl Into<String>) -> Check {
    Check {
        name: name.into(),
        health: Health::Ok,
        message: message.into(),
    }
}

fn warn(name: impl Into<String>, message: impl Into<String>) -> Check {
    Check {
        name: name.into(),
        health: Health::Warn,
        message: message.into(),
    }
}

fn fail(name: impl Into<String>, message: impl Into<String>) -> Check {
    Check {
        name: name.into(),
        health: Health::Fail,
        message: message.into(),
    }
}

pub fn cmd_doctor(root: &Path, db: &Path, plan_path: &Path) -> Result<()> {
    const EXPECTED_SCHEMA_VERSION: i64 = foundry_core::graph::LATEST_SCHEMA_VERSION;

    let mut checks = Vec::new();

    // 1. Database can be opened.
    let graph = match Graph::open(db) {
        Ok(g) => {
            checks.push(ok("db_open", "graph database opened"));
            g
        }
        Err(e) => {
            checks.push(fail("db_open", format!("cannot open database: {}", e)));
            print_report(&checks);
            return Ok(());
        }
    };

    // 2. WAL mode.
    match graph.wal_mode() {
        Ok(true) => checks.push(ok("wal_mode", "WAL journaling enabled")),
        Ok(false) => checks.push(warn("wal_mode", "WAL journaling not enabled")),
        Err(e) => checks.push(fail("wal_mode", format!("cannot read journal mode: {}", e))),
    }

    // 3. Schema version.
    match graph.schema_version() {
        Ok(v) if v == EXPECTED_SCHEMA_VERSION => {
            checks.push(ok("schema_version", format!("version {}", v)));
        }
        Ok(v) => checks.push(warn(
            "schema_version",
            format!("version {} (expected {})", v, EXPECTED_SCHEMA_VERSION),
        )),
        Err(e) => checks.push(fail("schema_version", format!("cannot read: {}", e))),
    }

    // 4. Stored migration checksums still match the canonical registry.
    match graph.verify_migration_checksums() {
        Ok(reports) => {
            let mismatches: Vec<String> = reports
                .iter()
                .filter_map(|report| match &report.status {
                    foundry_core::MigrationChecksumStatus::Mismatch { stored } => Some(format!(
                        "version {} stored checksum {} does not match the canonical registry",
                        report.version, stored
                    )),
                    _ => None,
                })
                .collect();
            let unknown: Vec<i64> = reports
                .iter()
                .filter(|report| {
                    report.status == foundry_core::MigrationChecksumStatus::UnknownVersion
                })
                .map(|report| report.version)
                .collect();
            if !mismatches.is_empty() {
                checks.push(fail("migration_checksums", mismatches.join("; ")));
            } else if !unknown.is_empty() {
                checks.push(warn(
                    "migration_checksums",
                    format!(
                        "versions {:?} are not in this binary's registry (newer database?)",
                        unknown
                    ),
                ));
            } else {
                checks.push(ok(
                    "migration_checksums",
                    format!("{} migration checksum(s) verified", reports.len()),
                ));
            }
        }
        Err(e) => checks.push(fail("migration_checksums", format!("cannot verify: {}", e))),
    }

    // 5. Graph in sync with filesystem.
    match reconcile_state(root, db) {
        Ok((true, _, _)) => checks.push(ok("filesystem_sync", "graph and filesystem in sync")),
        Ok((false, missing, extra)) => {
            let msg = format!(
                "drift: {} missing on disk, {} not in graph",
                missing.len(),
                extra.len()
            );
            checks.push(fail("filesystem_sync", msg));
        }
        Err(e) => checks.push(fail("filesystem_sync", format!("cannot reconcile: {}", e))),
    }

    // 6. Plan parseable.
    match fs::read_to_string(plan_path) {
        Ok(text) => match Plan::parse_path(plan_path, &text).first_undone() {
            Some(task) if task.stop.is_some() => checks.push(warn(
                "plan_state",
                format!(
                    "next task '{}' requires human stop approval",
                    task.description
                ),
            )),
            Some(_) => checks.push(ok("plan_state", "plan has runnable next task")),
            None => checks.push(ok("plan_state", "plan complete")),
        },
        Err(e) => checks.push(fail("plan_state", format!("cannot read plan: {}", e))),
    }

    // 7. Events recorded.
    match graph.events(1) {
        Ok(events) if !events.is_empty() => checks.push(ok("events", "events table has rows")),
        Ok(_) => checks.push(warn("events", "events table is empty")),
        Err(e) => checks.push(fail("events", format!("cannot read events: {}", e))),
    }

    // 8. Append-only event references remain joinable, or at least retain a
    // durable task key when a destructive rebuild replaced their node UUID.
    match graph.event_reference_integrity() {
        Ok(report) if report.missing_node_references == 0 => checks.push(ok(
            "event_references",
            format!(
                "{} event(s) scanned; no missing node references",
                report.events_scanned
            ),
        )),
        Ok(report) => checks.push(warn(
            "event_references",
            format!(
                "{} missing node reference(s) across {} event(s): {} task event(s) retain a durable key, {} event(s) are unresolvable",
                report.missing_node_references,
                report.events_with_missing_references,
                report.narratable_by_task_key,
                report.unresolvable_events
            ),
        )),
        Err(e) => checks.push(fail(
            "event_references",
            format!("cannot audit event references: {e}"),
        )),
    }

    // 9. Orphaned evidence old enough for the sweep to collect.
    match sweep::orphaned_blob_count(&graph) {
        Ok(0) => checks.push(ok(
            "orphaned_evidence",
            "no orphaned evidence blobs older than the sweep age guard",
        )),
        Ok(count) => checks.push(warn(
            "orphaned_evidence",
            format!("{count} orphaned evidence blob(s) older than the sweep age guard"),
        )),
        Err(e) => checks.push(fail(
            "orphaned_evidence",
            format!("cannot scan evidence blobs: {e}"),
        )),
    }

    // 10. Required tools on PATH.
    for tool in ["cargo", "just", "podman"] {
        if Command::new(tool).arg("--version").output().is_ok() {
            checks.push(ok(format!("tool_{}", tool), "found on PATH"));
        } else {
            checks.push(fail(format!("tool_{}", tool), "not found on PATH"));
        }
    }
    // 11. Optional local-model / network / sandbox tools.
    for tool in ["ollama", "curl", "bwrap"] {
        if Command::new(tool).arg("--version").output().is_ok() {
            checks.push(ok(format!("tool_{}", tool), "found on PATH"));
        } else {
            checks.push(warn(format!("tool_{}", tool), "not found on PATH"));
        }
    }

    print_report(&checks);

    let failures = checks.iter().filter(|c| c.health == Health::Fail).count();
    if failures > 0 {
        std::process::exit(1);
    }
    Ok(())
}

fn print_report(checks: &[Check]) {
    for check in checks {
        let symbol = match check.health {
            Health::Ok => "[OK]",
            Health::Warn => "[WARN]",
            Health::Fail => "[FAIL]",
        };
        println!("{} {}: {}", symbol, check.name, check.message);
    }
}

pub fn cmd_check_rules(db: &Path) -> Result<()> {
    let mut graph = Graph::open(db).with_context(|| format!("opening graph at {:?}", db))?;
    let rules = foundry_core::built_in_rules();

    // Upsert rules and detect any that are not yet approved.
    let unapproved = foundry_core::rules::upsert_rules(&mut graph, &rules)
        .with_context(|| "upserting rules into graph")?;

    if !unapproved.is_empty() {
        for rule_id in &unapproved {
            if let Some(node) = graph.find_node_by_name(NodeKind::Rule, rule_id)? {
                graph
                    .record_event(&Event::ReviewRequested {
                        review_id: node.id,
                        task_id: node.id,
                    })
                    .with_context(|| format!("recording review request for {}", rule_id))?;
            }
        }
        eprintln!("Review required for new/unapproved rules:");
        for rule_id in &unapproved {
            if let Ok(Some(node)) = graph.find_node_by_name(NodeKind::Rule, rule_id) {
                let name = node
                    .payload
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or(rule_id);
                eprintln!("  - {} ({})", rule_id, name);
            } else {
                eprintln!("  - {}", rule_id);
            }
        }
        eprintln!(
            "\nApprove each with: foundry approve-rule <rule-id> --db {:?}",
            db
        );
        bail!(
            "{} rule(s) pending approval; check-rules blocked.",
            unapproved.len()
        );
    }

    let mut failures = 0;
    let mut warnings = 0;

    for rule in &rules {
        let rule_result = match rule.check(&graph) {
            Ok(rr) => rr,
            Err(e) => RuleResult::Fail {
                reason: format!("rule engine error: {}", e),
            },
        };

        let rule_node = graph
            .find_node_by_name(NodeKind::Rule, rule.id())
            .with_context(|| format!("finding rule node for {}", rule.id()))?;
        let rule_id = match rule_node {
            Some(n) => n.id,
            None => {
                bail!("rule node {} missing after upsert", rule.id());
            }
        };
        graph
            .record_event(&Event::RuleTriggered {
                rule_id,
                result: rule_result.clone(),
            })
            .with_context(|| format!("recording rule event for {}", rule.id()))?;

        match rule_result {
            RuleResult::Pass => println!("[PASS] {} ({})", rule.name(), rule.id()),
            RuleResult::Warn { reason } => {
                println!("[WARN] {} ({}): {}", rule.name(), rule.id(), reason);
                warnings += 1;
            }
            RuleResult::Fail { reason } => {
                println!("[FAIL] {} ({}): {}", rule.name(), rule.id(), reason);
                failures += 1;
            }
        }
    }

    if failures > 0 {
        bail!("{} rule(s) failed", failures);
    }
    if warnings > 0 {
        println!("\n{} warning(s); no failures.", warnings);
    } else {
        println!("\nAll rules passed.");
    }
    Ok(())
}

pub fn cmd_approve_rule(db: &Path, rule_id: &str) -> Result<()> {
    let mut graph = Graph::open(db).with_context(|| format!("opening graph at {:?}", db))?;
    match foundry_core::rules::approve_rule(&mut graph, rule_id) {
        Ok(true) => {
            println!("Approved rule: {}", rule_id);
            Ok(())
        }
        Ok(false) => {
            bail!(
                "Rule '{}' not found in graph. Run `foundry check-rules` first to register it.",
                rule_id
            );
        }
        Err(e) => {
            bail!("Failed to approve rule '{}': {}", rule_id, e);
        }
    }
}

pub fn cmd_heal(root: &Path, db: &Path) -> Result<()> {
    let mutation = lease::acquire_repository(root, &lease::default_owner(), "heal")
        .map_err(|refusal| anyhow::anyhow!("{refusal}"))?;
    let (in_sync, _, _) = reconcile_state(root, db).context("initial reconcile")?;
    if in_sync {
        println!("Healthy: graph and filesystem are in sync.");
        return Ok(());
    }

    println!("Drift detected. Rebuilding graph from source...");
    indexer::rebuild_with_mutation(&mutation, root, db, false)?;

    let (in_sync_after, missing, extra) = reconcile_state(root, db).context("verify reconcile")?;
    if in_sync_after {
        println!("Healed: graph is now in sync with filesystem.");
        Ok(())
    } else {
        eprintln!("Heal failed: drift remains after rebuild.");
        for path in missing {
            eprintln!("  missing on disk: {}", path);
        }
        for path in extra {
            eprintln!("  not in graph: {}", path);
        }
        bail!("self-heal could not resolve drift")
    }
}

pub fn cmd_lease(root: &Path) -> Result<()> {
    match lease::inspect(&root.join(".foundry"))? {
        lease::LeaseStatus::Held(Some(info)) => {
            println!("Lease held (most recent Foundry metadata: {info})")
        }
        lease::LeaseStatus::Held(None) => println!("Lease held (holder recorded no metadata)"),
        lease::LeaseStatus::Free(Some(info)) => {
            println!("Lease free (most recent Foundry metadata: {info})")
        }
        lease::LeaseStatus::Free(None) => println!("Lease free"),
    }
    Ok(())
}

pub fn is_ignored(path: &Path) -> bool {
    let components: Vec<_> = path
        .components()
        .map(|c| c.as_os_str().to_string_lossy().to_string())
        .collect();
    components.iter().any(|c| {
        matches!(
            c.as_str(),
            ".git" | "target" | "node_modules" | ".foundry" | ".cache" | "dist" | "build"
        )
    })
}
