//! Retention enforcement: the delete path Foundry's governance envelopes
//! promise but nothing enforced until now. Evidence whose `RetentionPolicy`
//! is due for deletion is erased through lethe's erasure contract — two
//! stores, independent absence verification, content-minimized receipts.

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use foundry_core::{Disposition, Event, Graph, JobId, JobResultRow, TaskState};
use lethe::store::{
    ErasureAdapter, ErasureCoordinator, ErasureReportStatus, ErasureRequest, StoreCapabilities,
    StoreErasureResult, StoreErasureStatus,
};
use sha2::{Digest, Sha256};
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;

/// One governed result with a due disposition. Never carries content or
/// file paths — job ids point into retained append-only history.
#[derive(Debug, serde::Serialize)]
pub struct SweepItem {
    pub job_id: JobId,
    pub owner: String,
    pub policy: &'static str,
    pub due: DateTime<Utc>,
    /// Blob digests referenced by this result, captured before any deletion.
    #[serde(skip)]
    pub blob_manifest: Vec<String>,
}

#[derive(Debug, serde::Serialize)]
pub struct QuarantinedRow {
    pub job_id: String,
    pub error: String,
}

#[derive(Debug, Default, serde::Serialize)]
pub struct SweepClassification {
    pub delete_due: Vec<SweepItem>,
    /// Delete-due, but the owning task sits in review: retention never yanks
    /// evidence out from under an open human decision.
    pub deferred: Vec<SweepItem>,
    pub review_due: Vec<SweepItem>,
    pub retained: usize,
    pub preserved: usize,
    pub quarantined: Vec<QuarantinedRow>,
}

/// Classify every stored result by its governed disposition at `now`.
/// `review_pending` answers whether a task key currently sits in review.
pub fn classify(
    rows: Vec<JobResultRow>,
    review_pending: impl Fn(&str) -> bool,
    now: DateTime<Utc>,
) -> SweepClassification {
    let mut classification = SweepClassification::default();
    for row in rows {
        let result = match row.parsed {
            Ok(result) => result,
            Err(error) => {
                classification.quarantined.push(QuarantinedRow {
                    job_id: row.job_id,
                    error,
                });
                continue;
            }
        };
        let (policy, due) = match &result.governance.retention {
            foundry_core::RetentionPolicy::DeleteAfter { at } => ("delete_after", *at),
            foundry_core::RetentionPolicy::ReviewAfter { at } => ("review_after", *at),
            foundry_core::RetentionPolicy::Preserve { .. } => {
                classification.retained += 1;
                classification.preserved += 1;
                continue;
            }
        };
        let item = SweepItem {
            job_id: result.job_id,
            owner: result.governance.owner.clone(),
            policy,
            due,
            blob_manifest: blob_manifest(&result),
        };
        match result.governance.disposition_at(now) {
            Disposition::Retain => classification.retained += 1,
            Disposition::Review => classification.review_due.push(item),
            Disposition::Delete => {
                let pending = row.task_key.as_deref().is_some_and(&review_pending);
                if pending {
                    classification.deferred.push(item);
                } else {
                    classification.delete_due.push(item);
                }
            }
        }
    }
    classification
}

fn blob_manifest(result: &foundry_core::JobResult) -> Vec<String> {
    let mut manifest = BTreeSet::new();
    if let Some(change_set) = &result.change_set {
        for change in &change_set.files {
            for evidence in [change.before.as_ref(), change.after.as_ref()]
                .into_iter()
                .flatten()
            {
                if evidence.blob.is_some() {
                    manifest.insert(
                        evidence
                            .blob
                            .clone()
                            .unwrap_or_else(|| evidence.digest.clone()),
                    );
                }
            }
        }
    }
    manifest.into_iter().collect()
}

/// A one-way commitment over what a store erased: digests and ids feed the
/// hash but never appear in the receipt — a tombstone, not a backup.
fn store_receipt(
    store: &str,
    request: &ErasureRequest,
    erased: usize,
    deleted: &[String],
) -> String {
    let mut hasher = Sha256::new();
    for field in [store, &request.request_id, &request.subject_digest()] {
        hasher.update((field.len() as u64).to_le_bytes());
        hasher.update(field.as_bytes());
    }
    hasher.update((erased as u64).to_le_bytes());
    let mut sorted: Vec<&String> = deleted.iter().collect();
    sorted.sort();
    for id in sorted {
        hasher.update((id.len() as u64).to_le_bytes());
        hasher.update(id.as_bytes());
    }
    format!("foundry://erasure/{store}/sha256:{:x}", hasher.finalize())
}

fn adapter_error(kind: &str) -> lethe::store::AdapterError {
    lethe::store::AdapterError {
        kind: kind.to_string(),
    }
}

fn parse_subject(subject: &str) -> Result<JobId, lethe::store::AdapterError> {
    subject
        .parse::<uuid::Uuid>()
        .map(JobId)
        .map_err(|_| adapter_error("invalid_subject"))
}

/// Erases the `job_results` row for a subject job id.
struct JobResultsAdapter {
    graph: Rc<RefCell<Graph>>,
}

impl ErasureAdapter for JobResultsAdapter {
    fn name(&self) -> &str {
        "job-results"
    }

    fn capabilities(&self) -> StoreCapabilities {
        StoreCapabilities::new(false, false, true)
    }

    fn health(&mut self) -> Result<(), lethe::store::AdapterError> {
        self.graph
            .borrow()
            .job_result_exists(JobId(uuid::Uuid::nil()))
            .map(|_| ())
            .map_err(|_| adapter_error("storage_unavailable"))
    }

    fn erase_subject(
        &mut self,
        request: &ErasureRequest,
    ) -> Result<StoreErasureResult, lethe::store::AdapterError> {
        let job_id = parse_subject(&request.subject)?;
        let existed = self
            .graph
            .borrow_mut()
            .delete_job_result(job_id)
            .map_err(|_| adapter_error("delete_failed"))?;
        let erased = usize::from(existed);
        let receipt = store_receipt(
            self.name(),
            request,
            erased,
            std::slice::from_ref(&request.subject),
        );
        Ok(StoreErasureResult::successful(
            self.name(),
            request,
            erased,
            &receipt,
        ))
    }

    fn verify_subject_absent(&mut self, subject: &str) -> Result<bool, lethe::store::AdapterError> {
        let job_id = parse_subject(subject)?;
        self.graph
            .borrow()
            .job_result_exists(job_id)
            .map(|exists| !exists)
            .map_err(|_| adapter_error("verify_failed"))
    }
}

/// Erases a subject's evidence blobs, respecting shared references: a digest
/// still referenced by any remaining result is absent *for this subject* by
/// definition and must stay on disk.
struct BlobStoreAdapter {
    graph: Rc<RefCell<Graph>>,
    root: Option<PathBuf>,
    manifests: BTreeMap<String, Vec<String>>,
    /// Blobs younger than this are presumed to belong to a job that is still
    /// externalizing (bytes land before the row) and are neither deleted nor
    /// counted against verification. The same guard the orphan collector
    /// uses; a race can otherwise destroy a finishing job's fresh evidence.
    minimum_age: std::time::Duration,
}

impl BlobStoreAdapter {
    fn digest_path(root: &Path, digest: &str) -> Option<PathBuf> {
        let hex = digest.strip_prefix("sha256:")?;
        if hex.len() != 64 || !hex.chars().all(|character| character.is_ascii_hexdigit()) {
            return None;
        }
        Some(root.join(hex))
    }

    /// True when the file is old enough that no in-flight externalization
    /// can still own it. Erase and verify share this predicate so a young
    /// blob is consistently treated as out of scope for the subject.
    fn old_enough(&self, path: &Path) -> bool {
        std::fs::metadata(path)
            .and_then(|metadata| metadata.modified())
            .ok()
            .and_then(|modified| std::time::SystemTime::now().duration_since(modified).ok())
            .is_some_and(|age| age >= self.minimum_age)
    }

    fn still_referenced(&self) -> Result<BTreeSet<String>, lethe::store::AdapterError> {
        self.graph
            .borrow()
            .referenced_blob_digests()
            .map_err(|_| adapter_error("reference_scan_failed"))
    }
}

impl ErasureAdapter for BlobStoreAdapter {
    fn name(&self) -> &str {
        "blob-store"
    }

    fn capabilities(&self) -> StoreCapabilities {
        StoreCapabilities::new(false, false, true)
    }

    fn health(&mut self) -> Result<(), lethe::store::AdapterError> {
        // An absent blob root is healthy: nothing was ever externalized.
        Ok(())
    }

    fn erase_subject(
        &mut self,
        request: &ErasureRequest,
    ) -> Result<StoreErasureResult, lethe::store::AdapterError> {
        let Some(root) = self.root.clone() else {
            let receipt = store_receipt(self.name(), request, 0, &[]);
            return Ok(StoreErasureResult::successful(
                self.name(),
                request,
                0,
                &receipt,
            ));
        };
        let manifest = self
            .manifests
            .get(&request.subject)
            .cloned()
            .unwrap_or_default();
        let referenced = self.still_referenced()?;
        let mut deleted = Vec::new();
        for digest in &manifest {
            if referenced.contains(digest) {
                continue;
            }
            let Some(path) = Self::digest_path(&root, digest) else {
                continue;
            };
            if path.exists() && !self.old_enough(&path) {
                // A concurrent job may still own this byte-identical blob.
                continue;
            }
            match std::fs::remove_file(&path) {
                Ok(()) => deleted.push(digest.clone()),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(_) => return Err(adapter_error("blob_removal_failed")),
            }
        }
        let receipt = store_receipt(self.name(), request, deleted.len(), &deleted);
        Ok(StoreErasureResult::successful(
            self.name(),
            request,
            deleted.len(),
            &receipt,
        ))
    }

    fn verify_subject_absent(&mut self, subject: &str) -> Result<bool, lethe::store::AdapterError> {
        let Some(root) = self.root.clone() else {
            return Ok(true);
        };
        let manifest = match self.manifests.get(subject) {
            Some(manifest) => manifest.clone(),
            None => return Ok(true),
        };
        let referenced = self.still_referenced()?;
        for digest in &manifest {
            if referenced.contains(digest) {
                continue;
            }
            if let Some(path) = Self::digest_path(&root, digest)
                && path.exists()
                && self.old_enough(&path)
            {
                return Ok(false);
            }
        }
        Ok(true)
    }
}

/// Remove blob objects no remaining result references. Catches the crash
/// window between a row delete and its blob removal. The mtime guard keeps a
/// concurrently finishing job's freshly externalized blobs safe.
fn orphan_gc(graph: &Rc<RefCell<Graph>>, minimum_age: std::time::Duration) -> Result<usize> {
    let Some(root) = graph.borrow().blob_store_root()? else {
        return Ok(0);
    };
    if !root.exists() {
        return Ok(0);
    }
    let referenced = graph.borrow().referenced_blob_digests()?;
    let now = std::time::SystemTime::now();
    let mut removed = 0;
    for entry in std::fs::read_dir(&root).context("listing blob store")? {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        if name.len() != 64 || !name.chars().all(|character| character.is_ascii_hexdigit()) {
            continue;
        }
        if referenced.contains(&format!("sha256:{name}")) {
            continue;
        }
        let recent = entry
            .metadata()?
            .modified()
            .ok()
            .and_then(|modified| now.duration_since(modified).ok())
            .is_none_or(|age| age < minimum_age);
        if recent {
            continue;
        }
        std::fs::remove_file(entry.path()).context("removing orphaned blob")?;
        removed += 1;
    }
    Ok(removed)
}

fn print_items(label: &str, items: &[SweepItem]) {
    for item in items {
        println!(
            "  job {}  owner={}  policy={}  due={}  [{label}]",
            item.job_id.0,
            item.owner,
            item.policy,
            item.due.to_rfc3339()
        );
    }
}

pub fn run_sweep(db: &Path, enforce: bool, json: bool) -> Result<()> {
    let graph = Graph::open(db).with_context(|| format!("opening graph at {db:?}"))?;
    let graph = Rc::new(RefCell::new(graph));

    let rows = graph.borrow().job_result_rows()?;
    let review_pending = |task_key: &str| {
        graph
            .borrow()
            .task_state(task_key)
            .ok()
            .flatten()
            .is_some_and(|state| state == TaskState::Review)
    };
    let classification = classify(rows, review_pending, Utc::now());

    if !enforce {
        if json {
            println!("{}", serde_json::to_string_pretty(&classification)?);
        } else {
            println!(
                "Retention report (dry run): {} delete-due, {} deferred (review pending), {} review-due, {} retained ({} preserved), {} quarantined.",
                classification.delete_due.len(),
                classification.deferred.len(),
                classification.review_due.len(),
                classification.retained,
                classification.preserved,
                classification.quarantined.len(),
            );
            print_items("delete-due", &classification.delete_due);
            print_items("deferred: review pending", &classification.deferred);
            print_items("review-due", &classification.review_due);
            for row in &classification.quarantined {
                println!("  job {}  quarantined: {}", row.job_id, row.error);
            }
            if !classification.delete_due.is_empty() {
                println!("Run with --enforce to erase delete-due evidence.");
            }
        }
        return Ok(());
    }

    let manifests: BTreeMap<String, Vec<String>> = classification
        .delete_due
        .iter()
        .map(|item| (item.job_id.0.to_string(), item.blob_manifest.clone()))
        .collect();
    let root = graph.borrow().blob_store_root()?;
    let mut coordinator = ErasureCoordinator::new(vec![
        Box::new(JobResultsAdapter {
            graph: Rc::clone(&graph),
        }),
        Box::new(BlobStoreAdapter {
            graph: Rc::clone(&graph),
            root,
            manifests,
            minimum_age: std::time::Duration::from_secs(3600),
        }),
    ])
    .map_err(|error| anyhow::anyhow!("building erasure coordinator: {error:?}"))?;

    let mut deleted = 0;
    let mut incomplete = Vec::new();
    let mut receipts = Vec::new();
    for item in &classification.delete_due {
        let subject = item.job_id.0.to_string();
        let request = ErasureRequest::new(&format!("sweep-{subject}"), &subject)
            .map_err(|error| anyhow::anyhow!("building erasure request: {error:?}"))?;
        let report = coordinator.erase_subject(&request);
        let status = match report.status {
            ErasureReportStatus::Complete => {
                deleted += 1;
                "complete"
            }
            ErasureReportStatus::Partial => "partial",
            ErasureReportStatus::Failed => "failed",
        };
        if status != "complete" {
            incomplete.push(format!("job {subject}: {status}"));
        }
        let store_receipts: Vec<String> = report
            .stores
            .iter()
            .map(|store| {
                let store_status = match store.status {
                    StoreErasureStatus::Erased => "erased",
                    StoreErasureStatus::AlreadyAbsent => "already_absent",
                    StoreErasureStatus::Failed => "failed",
                    StoreErasureStatus::VerificationFailed => "verification_failed",
                };
                format!("{}:{}:{}", store.store, store_status, store.receipt)
            })
            .collect();
        graph
            .borrow_mut()
            .record_event(&Event::EvidenceErased {
                job_id: item.job_id,
                request_id: request.request_id.clone(),
                status: status.to_string(),
                receipt: report.receipt.clone(),
                store_receipts,
            })
            .context("recording evidence erased event")?;
        receipts.push((subject, status.to_string(), report.receipt));
    }

    let orphan_blobs_removed = orphan_gc(&graph, std::time::Duration::from_secs(3600))?;

    graph
        .borrow_mut()
        .record_event(&Event::RetentionSwept {
            enforced: true,
            delete_due: classification.delete_due.len(),
            deleted,
            deferred: classification.deferred.len(),
            review_due: classification.review_due.len(),
            retained: classification.retained,
            preserved: classification.preserved,
            quarantined: classification.quarantined.len(),
            orphan_blobs_removed,
        })
        .context("recording retention swept event")?;

    if json {
        println!(
            "{}",
            serde_json::json!({
                "enforced": true,
                "delete_due": classification.delete_due.len(),
                "deleted": deleted,
                "deferred": classification.deferred.len(),
                "review_due": classification.review_due.len(),
                "retained": classification.retained,
                "preserved": classification.preserved,
                "quarantined": classification.quarantined.len(),
                "orphan_blobs_removed": orphan_blobs_removed,
                "receipts": receipts
                    .iter()
                    .map(|(job, status, receipt)| serde_json::json!({
                        "job_id": job,
                        "status": status,
                        "receipt": receipt,
                    }))
                    .collect::<Vec<_>>(),
            })
        );
    } else {
        println!(
            "Retention sweep: {} erased, {} deferred (review pending), {} review-due, {} retained ({} preserved), {} quarantined, {} orphaned blob(s) collected.",
            deleted,
            classification.deferred.len(),
            classification.review_due.len(),
            classification.retained,
            classification.preserved,
            classification.quarantined.len(),
            orphan_blobs_removed,
        );
        for (job, status, receipt) in &receipts {
            println!("  job {job}  {status}  {receipt}");
        }
        print_items("review-due", &classification.review_due);
    }

    if !incomplete.is_empty() {
        bail!(
            "retention sweep left {} request(s) incomplete (events recorded):\n{}",
            incomplete.len(),
            incomplete.join("\n")
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use foundry_core::{
        ChangeSet, ChangeStatus, ChangedFile, FileEvidence, GovernanceEnvelope, JobResult,
        JobState, KnowledgeLayer, RetentionPolicy, SourceRef, Transformation,
    };

    fn governance(retention: RetentionPolicy) -> GovernanceEnvelope {
        GovernanceEnvelope {
            layer: KnowledgeLayer::Observed,
            sources: vec![SourceRef {
                uri: "job://test/output".into(),
                digest: None,
            }],
            assumptions: Vec::new(),
            transformation: Transformation {
                name: "capture-job-result".into(),
                version: "1".into(),
                input_digests: Vec::new(),
            },
            owner: "sweep-tests".into(),
            retention,
        }
    }

    fn recorded_result(
        graph: &mut Graph,
        task_key: &str,
        idempotency: &str,
        retention: RetentionPolicy,
        contents: &[&[u8]],
    ) -> JobResult {
        let job = graph.create_job(task_key, idempotency).unwrap();
        graph.transition_job(job.id, JobState::Running).unwrap();
        graph.transition_job(job.id, JobState::Succeeded).unwrap();
        let mut result =
            JobResult::new(job.id, JobState::Succeeded, governance(retention)).unwrap();
        if !contents.is_empty() {
            result.change_set = Some(ChangeSet {
                base_revision: "sha256:base".into(),
                patch_digest: "sha256:patch".into(),
                files: contents
                    .iter()
                    .enumerate()
                    .map(|(index, bytes)| ChangedFile {
                        path: format!("evidence-{index}.txt"),
                        status: ChangeStatus::Added,
                        before: None,
                        after: Some(FileEvidence {
                            digest: format!("sha256:{:x}", sha2::Sha256::digest(bytes)),
                            bytes: bytes.to_vec(),
                            blob: None,
                            executable: false,
                        }),
                    })
                    .collect(),
            });
        }
        graph.record_job_result(&result).unwrap();
        result
    }

    fn synthetic_row(task_key: &str, retention: RetentionPolicy) -> JobResultRow {
        let result =
            JobResult::new(JobId::new(), JobState::Succeeded, governance(retention)).unwrap();
        JobResultRow {
            job_id: result.job_id.0.to_string(),
            task_key: Some(task_key.to_string()),
            created_at: "2026-01-01T00:00:00Z".into(),
            raw: serde_json::to_string(&result).unwrap(),
            parsed: Ok(result),
        }
    }

    fn past() -> DateTime<Utc> {
        Utc::now() - chrono::TimeDelta::days(1)
    }

    fn future() -> DateTime<Utc> {
        Utc::now() + chrono::TimeDelta::days(30)
    }

    #[test]
    fn classification_covers_every_bucket() {
        let rows = vec![
            synthetic_row("task-delete", RetentionPolicy::DeleteAfter { at: past() }),
            synthetic_row("task-deferred", RetentionPolicy::DeleteAfter { at: past() }),
            synthetic_row("task-review", RetentionPolicy::ReviewAfter { at: past() }),
            synthetic_row(
                "task-retained",
                RetentionPolicy::DeleteAfter { at: future() },
            ),
            synthetic_row(
                "task-preserved",
                RetentionPolicy::Preserve {
                    basis: "incident evidence".into(),
                },
            ),
            JobResultRow {
                job_id: "corrupt-row".into(),
                task_key: Some("task-corrupt".into()),
                created_at: "2026-01-01T00:00:00Z".into(),
                raw: "{\"broken\": tru".into(),
                parsed: Err("expected value at line 1".into()),
            },
        ];
        let classification = classify(rows, |task_key| task_key == "task-deferred", Utc::now());
        assert_eq!(classification.delete_due.len(), 1);
        assert_eq!(classification.delete_due[0].owner, "sweep-tests");
        assert_eq!(classification.deferred.len(), 1);
        assert_eq!(classification.review_due.len(), 1);
        assert_eq!(classification.retained, 2);
        assert_eq!(classification.preserved, 1);
        assert_eq!(classification.quarantined.len(), 1);
        assert_eq!(classification.quarantined[0].job_id, "corrupt-row");
    }

    #[test]
    fn erasure_respects_shared_blobs_and_replays_idempotently() {
        let root = std::env::temp_dir().join(format!("foundry-sweep-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let graph = Graph::open(&root.join("db.sqlite")).unwrap();
        let graph = Rc::new(RefCell::new(graph));

        // The doomed result references an exclusive blob AND a blob shared
        // with the surviving result: only the exclusive one may die.
        let shared: &[u8] = b"shared evidence bytes";
        let exclusive: &[u8] = b"exclusive evidence bytes";
        let (doomed, doomed_manifest) = {
            let mut graph = graph.borrow_mut();
            let doomed = recorded_result(
                &mut graph,
                "task-doomed",
                "a",
                RetentionPolicy::DeleteAfter { at: past() },
                &[exclusive, shared],
            );
            recorded_result(
                &mut graph,
                "task-survivor",
                "b",
                RetentionPolicy::DeleteAfter { at: future() },
                &[shared],
            );
            // The manifest comes from the stored (externalized) row, exactly
            // as the driver captures it before deletion.
            let manifest = graph
                .job_result_rows()
                .unwrap()
                .into_iter()
                .find(|row| row.job_id == doomed.job_id.0.to_string())
                .and_then(|row| row.parsed.ok())
                .map(|result| blob_manifest(&result))
                .unwrap();
            assert_eq!(manifest.len(), 2, "manifest: {manifest:?}");
            (doomed, manifest)
        };

        let subject = doomed.job_id.0.to_string();
        let manifests = BTreeMap::from([(subject.clone(), doomed_manifest.clone())]);
        let blob_root = graph.borrow().blob_store_root().unwrap();
        let mut coordinator = ErasureCoordinator::new(vec![
            Box::new(JobResultsAdapter {
                graph: Rc::clone(&graph),
            }),
            Box::new(BlobStoreAdapter {
                graph: Rc::clone(&graph),
                root: blob_root.clone(),
                manifests: manifests.clone(),
                minimum_age: std::time::Duration::ZERO,
            }),
        ])
        .unwrap();

        let request = ErasureRequest::new(&format!("sweep-{subject}"), &subject).unwrap();
        let report = coordinator.erase_subject(&request);
        assert!(
            matches!(report.status, ErasureReportStatus::Complete),
            "report: {report:?}"
        );
        assert!(!graph.borrow().job_result_exists(doomed.job_id).unwrap());

        // The doomed result's exclusive blob is gone; the shared blob is
        // still referenced by the survivor and must remain on disk.
        let exclusive_digest = format!("sha256:{:x}", sha2::Sha256::digest(exclusive));
        let exclusive_path =
            BlobStoreAdapter::digest_path(blob_root.as_ref().unwrap(), &exclusive_digest).unwrap();
        assert!(!exclusive_path.exists(), "exclusive blob must be erased");
        let shared_digest = format!("sha256:{:x}", sha2::Sha256::digest(shared));
        let shared_path =
            BlobStoreAdapter::digest_path(blob_root.as_ref().unwrap(), &shared_digest).unwrap();
        assert!(shared_path.exists(), "shared blob must survive");
        let survivor_results = graph
            .borrow()
            .job_results_for_task("task-survivor")
            .unwrap();
        assert_eq!(survivor_results.len(), 1, "survivor must still hydrate");

        // Receipts are commitments: no blob digest, no owner text.
        for store in &report.stores {
            assert!(!store.receipt.contains(&exclusive_digest["sha256:".len()..]));
            assert!(!store.receipt.contains("sweep-tests"));
        }

        // Replay in a fresh coordinator converges to AlreadyAbsent + Complete.
        let mut replay_coordinator = ErasureCoordinator::new(vec![
            Box::new(JobResultsAdapter {
                graph: Rc::clone(&graph),
            }),
            Box::new(BlobStoreAdapter {
                graph: Rc::clone(&graph),
                root: blob_root,
                manifests,
                minimum_age: std::time::Duration::ZERO,
            }),
        ])
        .unwrap();
        let replay = replay_coordinator.erase_subject(&request);
        assert!(matches!(replay.status, ErasureReportStatus::Complete));
        assert!(
            replay
                .stores
                .iter()
                .all(|store| matches!(store.status, StoreErasureStatus::AlreadyAbsent)),
            "replay: {replay:?}"
        );

        drop(replay_coordinator);
        drop(coordinator);
        drop(graph);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn orphan_gc_removes_only_old_unreferenced_blobs() {
        let root = std::env::temp_dir().join(format!("foundry-gc-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let graph = Graph::open(&root.join("db.sqlite")).unwrap();
        let graph = Rc::new(RefCell::new(graph));
        recorded_result(
            &mut graph.borrow_mut(),
            "task-live",
            "a",
            RetentionPolicy::ReviewAfter { at: future() },
            &[b"live bytes"],
        );
        let blob_root = graph.borrow().blob_store_root().unwrap().unwrap();
        let orphan = blob_root.join("f".repeat(64));
        std::fs::write(&orphan, b"orphan").unwrap();

        // A zero minimum age collects the orphan immediately; the referenced
        // blob stays.
        let removed = orphan_gc(&graph, std::time::Duration::ZERO).unwrap();
        assert_eq!(removed, 1);
        assert!(!orphan.exists());
        let live_digest = format!("{:x}", sha2::Sha256::digest(b"live bytes"));
        assert!(blob_root.join(live_digest).exists());

        // A fresh orphan under the age guard survives.
        let young = blob_root.join("e".repeat(64));
        std::fs::write(&young, b"young orphan").unwrap();
        let removed = orphan_gc(&graph, std::time::Duration::from_secs(3600)).unwrap();
        assert_eq!(removed, 0);
        assert!(young.exists());

        drop(graph);
        std::fs::remove_dir_all(root).unwrap();
    }
}
