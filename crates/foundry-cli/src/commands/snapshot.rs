use anyhow::{Context, Result, bail};
use clap::Subcommand;
use foundry_core::{Event, Graph};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::lease;

#[derive(Subcommand)]
pub enum SnapshotAction {
    /// Create a snapshot of the database.
    Create {
        /// Project root.
        #[arg(long, default_value = ".")]
        root: PathBuf,
        /// Path to the SQLite database.
        #[arg(long, default_value = "./.foundry/db.sqlite")]
        db: PathBuf,
        /// Snapshot name. Defaults to an ISO-8601 timestamp.
        name: Option<String>,
    },
    /// List available snapshots.
    List {
        /// Project root.
        #[arg(long, default_value = ".")]
        root: PathBuf,
        /// Path to the SQLite database.
        #[arg(long, default_value = "./.foundry/db.sqlite")]
        db: PathBuf,
    },
    /// Restore a snapshot over the current database.
    Restore {
        /// Project root.
        #[arg(long, default_value = ".")]
        root: PathBuf,
        /// Path to the SQLite database.
        #[arg(long, default_value = "./.foundry/db.sqlite")]
        db: PathBuf,
        /// Snapshot name to restore.
        name: String,
        /// Skip confirmation prompt.
        #[arg(long)]
        force: bool,
    },
}

pub fn cmd_snapshot(action: SnapshotAction) -> Result<()> {
    match action {
        SnapshotAction::Create { root, db, name } => cmd_snapshot_create(&root, &db, name),
        SnapshotAction::List { root, db } => cmd_snapshot_list(&root, &db),
        SnapshotAction::Restore {
            root,
            db,
            name,
            force,
        } => cmd_snapshot_restore(&root, &db, &name, force),
    }
}

fn snapshot_dir(db: &Path) -> PathBuf {
    db.parent()
        .map(|p| p.join("snapshots"))
        .unwrap_or_else(|| PathBuf::from("snapshots"))
}

fn snapshot_path(db: &Path, name: &str) -> Result<PathBuf> {
    let mut components = Path::new(name).components();
    let safe = matches!(components.next(), Some(std::path::Component::Normal(_)))
        && components.next().is_none();
    if !safe {
        bail!("snapshot name must be one non-empty path component");
    }
    Ok(snapshot_dir(db).join(format!("{name}.sqlite")))
}

fn cmd_snapshot_create(root: &Path, db: &Path, name: Option<String>) -> Result<()> {
    let mutation = lease::acquire_repository(root, &lease::default_owner(), "snapshot create")
        .map_err(|refusal| anyhow::anyhow!("{refusal}"))?;
    mutation.require_path(db)?;
    let graph = Graph::open(db).with_context(|| format!("opening graph at {:?}", db))?;
    // Closing a connection only checkpoints when it is the last one anywhere;
    // checkpoint explicitly so the copy below includes transactions still in
    // the WAL even while other foundry processes hold the database open.
    graph
        .checkpoint_wal()
        .with_context(|| format!("checkpointing WAL of {:?} before snapshot", db))?;
    drop(graph);

    let name = name.unwrap_or_else(|| chrono::Local::now().format("%Y%m%d-%H%M%S").to_string());
    let dest = snapshot_path(db, &name)?;
    let dir = dest.parent().context("snapshot path has no parent")?;
    fs::create_dir_all(dir).with_context(|| format!("creating snapshot directory {:?}", dir))?;
    if dest.exists() {
        bail!("snapshot already exists: {}", dest.display());
    }

    fs::copy(db, &dest).with_context(|| format!("copying {:?} to {:?}", db, dest))?;
    // The snapshot is a full copy of the graph; hold it at the same privacy
    // the lease enforces (0700 dir, 0600 file) from the moment it exists
    // instead of waiting for the next acquisition to re-harden it.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
            .with_context(|| format!("restricting snapshot directory {:?}", dir))?;
        fs::set_permissions(&dest, fs::Permissions::from_mode(0o600))
            .with_context(|| format!("restricting snapshot {:?}", dest))?;
    }

    let mut graph = Graph::open(db).with_context(|| format!("reopening graph at {:?}", db))?;
    graph.record_event(&Event::SnapshotCreated {
        name: name.clone(),
        path: dest.to_string_lossy().to_string(),
    })?;

    println!("Created snapshot: {} -> {:?}", name, dest);
    Ok(())
}

fn cmd_snapshot_list(_root: &Path, db: &Path) -> Result<()> {
    let dir = snapshot_dir(db);
    if !dir.exists() {
        println!("No snapshots found.");
        return Ok(());
    }

    let mut entries: Vec<_> = fs::read_dir(&dir)
        .with_context(|| format!("reading snapshot directory {:?}", dir))?
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.path()
                .extension()
                .map(|ext| ext == "sqlite")
                .unwrap_or(false)
        })
        .collect();

    if entries.is_empty() {
        println!("No snapshots found.");
        return Ok(());
    }

    entries.sort_by_key(|e| {
        e.metadata()
            .and_then(|m| m.modified())
            .unwrap_or(std::time::UNIX_EPOCH)
    });

    for entry in entries {
        println!("{}", entry.file_name().to_string_lossy());
    }
    Ok(())
}

fn cmd_snapshot_restore(root: &Path, db: &Path, name: &str, force: bool) -> Result<()> {
    let src = snapshot_path(db, name)?;
    if !src.exists() {
        bail!("snapshot not found: {:?}", src);
    }

    if !force {
        print!(
            "This will overwrite {:?} with {:?}. Proceed? [y/N]: ",
            db, src
        );
        std::io::stdout().flush()?;
        let mut choice = String::new();
        std::io::stdin().read_line(&mut choice)?;
        if !matches!(choice.trim().to_lowercase().as_str(), "y" | "yes") {
            println!("Restore cancelled.");
            return Ok(());
        }
    }
    let mutation = lease::acquire_repository(root, &lease::default_owner(), "snapshot restore")
        .map_err(|refusal| anyhow::anyhow!("{refusal}"))?;
    mutation.require_path(db)?;
    mutation.require_path(&src)?;
    // Restoring rewinds history wholesale: evidence erased after this
    // snapshot reappears as rows (their erasure receipts vanish with the
    // rewind, and rows whose blobs were already collected become
    // unhydratable). Retention enforcement does not survive a restore —
    // re-run `sweep --enforce` afterwards and treat pre-restore erasure
    // receipts as claims about a timeline this database no longer has.
    println!(
        "warning: restore rewinds retention history; evidence erased after the snapshot will reappear. Re-run `foundry sweep --enforce` after restoring."
    );

    let mut graph = Graph::open(db).with_context(|| format!("opening graph at {:?}", db))?;
    graph
        .restore_from_snapshot(&src)
        .with_context(|| format!("restoring validated snapshot {:?}", src))?;
    graph.record_event(&Event::SnapshotRestored {
        name: name.to_string(),
        path: src.to_string_lossy().to_string(),
    })?;

    println!("Restored snapshot {} to {:?}", name, db);
    Ok(())
}
