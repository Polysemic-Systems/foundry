//! SQLite schema for migration bookkeeping.

use rusqlite::Connection;

/// Ensures the `schema_migrations` table exists with the canonical shape.
///
/// The table stores the migration version number as the primary key, the
/// SHA-256 checksum of the applied migration as a required text value, and
/// the timestamp when the migration was applied.
///
/// Legacy databases whose `schema_migrations` table has a different column
/// order or constraints are recreated in place while preserving `version`,
/// `applied_at`, and any existing `checksum` value.
pub fn ensure_schema_migrations_table(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute(
        "CREATE TABLE IF NOT EXISTS schema_migrations (
            version INTEGER PRIMARY KEY,
            checksum TEXT NOT NULL,
            applied_at TEXT NOT NULL
        )",
        [],
    )?;

    let table_info = {
        let mut stmt = conn.prepare("PRAGMA table_info(schema_migrations)")?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, bool>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, bool>(5)?,
            ))
        })?;
        rows.collect::<Result<Vec<_>, _>>()?
    };

    let canonical = vec![
        (
            "version".to_string(),
            "INTEGER".to_string(),
            false,
            None,
            true,
        ),
        (
            "checksum".to_string(),
            "TEXT".to_string(),
            true,
            None,
            false,
        ),
        (
            "applied_at".to_string(),
            "TEXT".to_string(),
            true,
            None,
            false,
        ),
    ];

    if table_info != canonical {
        conn.execute(
            "ALTER TABLE schema_migrations RENAME TO schema_migrations_old",
            [],
        )?;
        conn.execute(
            "CREATE TABLE schema_migrations (
                version INTEGER PRIMARY KEY,
                checksum TEXT NOT NULL,
                applied_at TEXT NOT NULL
            )",
            [],
        )?;

        let old_columns: Vec<String> = {
            let mut stmt = conn.prepare("PRAGMA table_info(schema_migrations_old)")?;
            let rows = stmt.query_map([], |row| {
                let name: String = row.get(1)?;
                Ok(name)
            })?;
            rows.collect::<Result<Vec<_>, _>>()?
        };

        let checksum_expr = if old_columns.iter().any(|name| name == "checksum") {
            "COALESCE(checksum, '')"
        } else {
            "''"
        };
        let applied_at_expr = if old_columns.iter().any(|name| name == "applied_at") {
            "COALESCE(applied_at, '')"
        } else {
            "''"
        };

        conn.execute(
            &format!(
                "INSERT INTO schema_migrations (version, checksum, applied_at)
                 SELECT version, {checksum_expr}, {applied_at_expr}
                 FROM schema_migrations_old"
            ),
            [],
        )?;
        conn.execute("DROP TABLE schema_migrations_old", [])?;
    }

    Ok(())
}
