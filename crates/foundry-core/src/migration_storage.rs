//! Persistent storage for applied migration versions and their checksums.

use chrono::Utc;
use rusqlite::Connection;

/// Errors that can occur when interacting with migration storage.
#[derive(Debug, thiserror::Error)]
pub enum MigrationStorageError {
    #[error("invalid SHA-256 checksum: {0}")]
    InvalidChecksum(String),
    #[error("unknown migration version: {0}")]
    UnknownVersion(i64),
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
}

/// Storage for applied migration versions and their SHA-256 checksums.
pub struct MigrationStorage {
    conn: Connection,
}

impl MigrationStorage {
    /// Opens an in-memory store and ensures the schema migrations table exists.
    pub fn open_in_memory() -> Result<Self, MigrationStorageError> {
        let conn = Connection::open_in_memory()?;
        crate::db::schema::ensure_schema_migrations_table(&conn)?;
        Ok(Self { conn })
    }

    /// Records a migration version and its canonical SHA-256 checksum.
    ///
    /// The checksum must match the canonical migration SQL for the version in
    /// the shared registry. Arbitrary caller-supplied digests are rejected so
    /// that `Graph::migrate` and `MigrationStorage` share one cryptographic
    /// binding.
    pub fn record(&mut self, version: i64, checksum: &str) -> Result<(), MigrationStorageError> {
        if !is_valid_sha256_checksum(checksum) {
            return Err(MigrationStorageError::InvalidChecksum(checksum.to_string()));
        }
        let canonical = crate::migration_registry::checksum_for(version)
            .ok_or(MigrationStorageError::UnknownVersion(version))?;
        if checksum != canonical {
            return Err(MigrationStorageError::InvalidChecksum(checksum.to_string()));
        }
        self.conn.execute(
            "INSERT INTO schema_migrations (version, checksum, applied_at) VALUES (?1, ?2, ?3)",
            (version, checksum, Utc::now().to_rfc3339()),
        )?;
        Ok(())
    }

    /// Records a migration version using the canonical SHA-256 checksum computed
    /// from the migration SQL in the shared registry.
    ///
    /// This method rejects arbitrary caller-supplied digests by computing the
    /// checksum internally, strengthening the cryptographic binding between
    /// `Graph::migrate` and `MigrationStorage`.
    pub fn record_canonical(&mut self, version: i64) -> Result<(), MigrationStorageError> {
        let checksum = crate::migration_registry::checksum_for(version)
            .ok_or(MigrationStorageError::UnknownVersion(version))?;
        self.record(version, &checksum)
    }

    /// Retrieves the checksum for a given migration version, if any.
    pub fn checksum_for(&self, version: i64) -> Result<Option<String>, MigrationStorageError> {
        let mut stmt = self
            .conn
            .prepare("SELECT checksum FROM schema_migrations WHERE version = ?1")?;
        let mut rows = stmt.query([version])?;
        if let Some(row) = rows.next()? {
            Ok(Some(row.get(0)?))
        } else {
            Ok(None)
        }
    }
}

fn is_valid_sha256_checksum(checksum: &str) -> bool {
    checksum.len() == 64 && checksum.chars().all(|c| c.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use crate::migration_storage::MigrationStorage;

    impl MigrationStorage {
        /// Test-only access to the underlying SQLite connection for verifying schema shape.
        pub(crate) fn connection(&self) -> &rusqlite::Connection {
            &self.conn
        }
    }

    #[test]
    fn records_and_retrieves_sha256_checksum_by_version() {
        let mut storage = MigrationStorage::open_in_memory().unwrap();
        let checksum = crate::migration_registry::checksum_for(1)
            .expect("version 1 must have a canonical checksum");
        storage.record(1, &checksum).unwrap();

        assert_eq!(storage.checksum_for(1).unwrap(), Some(checksum));
        assert_eq!(storage.checksum_for(2).unwrap(), None);
    }

    #[test]
    fn rejects_checksums_that_are_not_64_character_hex() {
        let mut storage = MigrationStorage::open_in_memory().unwrap();
        assert!(storage.record(1, "not-a-checksum").is_err());
        assert!(storage.record(1, "abcd").is_err());
    }

    /// Regression: MigrationStorage must not accept arbitrary caller-supplied checksums,
    /// even when they are syntactically valid SHA-256 digests. The stored checksum must be
    /// the SHA-256 of the canonical migration SQL for the version, so that Graph::migrate
    /// and MigrationStorage share the same cryptographic binding.
    #[test]
    fn record_rejects_valid_hex_checksum_that_does_not_match_canonical_sql() {
        let mut storage = MigrationStorage::open_in_memory().unwrap();
        let canonical = crate::migration_registry::checksum_for(1)
            .expect("version 1 must have a canonical checksum");

        let mut wrong = canonical.clone();
        let last = wrong.pop().expect("checksum is non-empty");
        wrong.push(if last == '0' { '1' } else { '0' });
        assert_ne!(
            wrong, canonical,
            "the altered checksum must differ from the canonical one"
        );

        assert!(
            storage.record(1, &wrong).is_err(),
            "MigrationStorage must reject a valid 64-character hex checksum that does not match the canonical migration SQL"
        );
    }

    /// Regression: MigrationStorage must refuse to record a version that has no canonical
    /// migration content. Otherwise it can store a checksum for migration SQL that is not
    /// part of the shared registry, breaking the contract that Graph::migrate enforces.
    #[test]
    fn record_rejects_unknown_migration_version() {
        let mut storage = MigrationStorage::open_in_memory().unwrap();
        let checksum = "0000000000000000000000000000000000000000000000000000000000000001";
        assert!(
            storage.record(9999, checksum).is_err(),
            "MigrationStorage must reject a version that is not present in the canonical migration registry"
        );
    }

    /// Regression: MigrationStorage must not define a schema_migrations table that is
    /// incompatible with Graph::migrate. The exported API previously omitted applied_at,
    /// creating a parallel table schema that could not interoperate with the graph.
    #[test]
    fn migration_storage_schema_includes_applied_at_for_graph_compatibility() {
        let storage = MigrationStorage::open_in_memory().unwrap();
        let mut stmt = storage
            .conn
            .prepare("PRAGMA table_info(schema_migrations)")
            .unwrap();
        let columns = stmt
            .query_map([], |row| {
                let name: String = row.get(1)?;
                let typ: String = row.get(2)?;
                let notnull: bool = row.get(3)?;
                let pk: bool = row.get(5)?;
                Ok((name, typ, notnull, pk))
            })
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();

        assert!(
            columns.contains(&("applied_at".into(), "TEXT".into(), true, false)),
            "MigrationStorage.schema_migrations must include applied_at TEXT NOT NULL, \
             matching the schema used by Graph::migrate"
        );
    }

    /// Regression (reviewer feedback): MigrationStorage must compute the canonical SHA-256
    /// checksum from the migration SQL rather than accept a digest from callers. A storage
    /// API that takes a caller-supplied checksum, even with validation, leaves room for a
    /// weak implementation that stores arbitrary digests on the success path.
    #[test]
    fn migration_storage_records_checksum_computed_from_canonical_sql() {
        use sha2::{Digest, Sha256};

        let mut storage = MigrationStorage::open_in_memory().unwrap();
        storage.record_canonical(1).unwrap();

        let sql = crate::migration_registry::migrations()
            .into_iter()
            .find(|(v, _)| *v == 1)
            .map(|(_, sql)| sql)
            .expect("version 1 must be present in the canonical migration registry");
        let expected = format!("{:x}", Sha256::digest(sql.as_bytes()));

        assert_eq!(
            storage.checksum_for(1).unwrap(),
            Some(expected),
            "MigrationStorage must store the SHA-256 digest of the canonical migration SQL, \
             computed by the storage itself"
        );
    }

    /// Regression (reviewer feedback): The canonical-recording API must remain guarded by
    /// the registry. A version with no known migration content must be rejected rather than
    /// recorded with a fabricated or empty checksum.
    #[test]
    fn record_canonical_rejects_unknown_migration_version() {
        let mut storage = MigrationStorage::open_in_memory().unwrap();
        assert!(
            storage.record_canonical(9999).is_err(),
            "record_canonical must reject a version that is not present in the canonical migration registry"
        );
    }
}
