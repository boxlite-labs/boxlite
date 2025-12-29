//! Database layer for boxlite.
//!
//! Provides SQLite-based persistence using Podman-style pattern:
//! - BoxConfig: Immutable configuration (stored once at creation)
//! - BoxState: Mutable state (updated during lifecycle)
//!
//! Uses JSON blob pattern for flexibility with queryable columns for performance.

mod boxes;
mod schema;

use std::path::Path;
use std::sync::Arc;

use chrono::Utc;
use parking_lot::{Mutex, MutexGuard};
use rusqlite::{Connection, OptionalExtension};

use boxlite_shared::errors::{BoxliteError, BoxliteResult};

pub use boxes::BoxStore;

/// Helper macro to convert rusqlite errors to BoxliteError.
macro_rules! db_err {
    ($result:expr) => {
        $result.map_err(|e| BoxliteError::Database(e.to_string()))
    };
}

pub(crate) use db_err;

/// SQLite database handle.
///
/// Thread-safe via `parking_lot::Mutex`. Domain-specific stores
/// wrap this to provide their APIs (e.g., `BoxMetadataStore`).
#[derive(Clone)]
pub struct Database {
    conn: Arc<Mutex<Connection>>,
}

impl Database {
    /// Open or create the database.
    pub fn open(db_path: &Path) -> BoxliteResult<Self> {
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let conn = db_err!(Connection::open(db_path))?;

        // SQLite configuration (matches Podman patterns)
        // - WAL mode: Better concurrent read performance
        // - FULL sync: Maximum durability (fsync after each transaction)
        // - Foreign keys: Referential integrity
        // - Busy timeout: 100s to handle long operations (Podman uses 100s)
        db_err!(conn.execute_batch(
            "
            PRAGMA journal_mode=WAL;
            PRAGMA synchronous=FULL;
            PRAGMA foreign_keys=ON;
            PRAGMA busy_timeout=100000;
            "
        ))?;

        Self::init_schema(&conn)?;

        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// Acquire the database connection.
    pub(crate) fn conn(&self) -> MutexGuard<'_, Connection> {
        self.conn.lock()
    }

    /// Initialize database schema.
    ///
    /// Order of operations:
    /// 1. Create schema_version table (safe, no dependencies)
    /// 2. Check current version
    /// 3. New DB: apply full schema
    ///    Existing DB with older version: run migrations
    ///    Existing DB with newer version: error (need newer boxlite)
    ///    Existing DB with same version: nothing to do
    fn init_schema(conn: &Connection) -> BoxliteResult<()> {
        // Step 1: Create schema_version table first (always safe)
        db_err!(conn.execute_batch(schema::SCHEMA_VERSION_TABLE))?;

        // Step 2: Check current version
        let current_version: Option<i32> = db_err!(
            conn.query_row(
                "SELECT version FROM schema_version WHERE id = 1",
                [],
                |row| row.get(0),
            )
            .optional()
        )?;

        match current_version {
            None => {
                // New database - apply full latest schema
                Self::apply_full_schema(conn)?;
            }
            Some(v) if v == schema::SCHEMA_VERSION => {
                // Already at current version - nothing to do
            }
            Some(v) => {
                // Strict version check: any mismatch is an error
                return Err(BoxliteError::Database(format!(
                    "Schema version mismatch: database has v{}, process expects v{}. \
                     Run `boxlite migrate` or use matching boxlite version.",
                    v,
                    schema::SCHEMA_VERSION
                )));
            }
        }

        Ok(())
    }

    /// Apply full schema for new database.
    fn apply_full_schema(conn: &Connection) -> BoxliteResult<()> {
        for sql in schema::all_schemas() {
            db_err!(conn.execute_batch(sql))?;
        }

        let now = Utc::now().to_rfc3339();
        db_err!(conn.execute(
            "INSERT INTO schema_version (id, version, updated_at) VALUES (1, ?1, ?2)",
            rusqlite::params![schema::SCHEMA_VERSION, now],
        ))?;

        tracing::info!(
            "Initialized database schema version {}",
            schema::SCHEMA_VERSION
        );
        Ok(())
    }

    /// Run migrations from `from_version` to current schema version.
    ///
    /// Called by explicit `boxlite migrate` command, not automatically.
    #[allow(dead_code)] // Will be used by CLI migrate command
    fn run_migrations(conn: &Connection, from_version: i32) -> BoxliteResult<()> {
        let mut current = from_version;

        // Migration 2 -> 3: Add name column with UNIQUE constraint
        if current == 2 {
            tracing::info!("Running migration 2 -> 3: Adding name column to box_config");

            // Add name column
            db_err!(conn.execute_batch("ALTER TABLE box_config ADD COLUMN name TEXT;"))?;

            // Create unique index (enforces uniqueness, allows multiple NULLs)
            db_err!(conn.execute_batch(
                "CREATE UNIQUE INDEX IF NOT EXISTS idx_box_config_name_unique ON box_config(name);"
            ))?;

            // Populate name from JSON for existing rows
            db_err!(conn.execute_batch(
                "UPDATE box_config SET name = json_extract(json, '$.name') WHERE name IS NULL;"
            ))?;

            current = 3;
        }

        // Update schema version
        let now = Utc::now().to_rfc3339();
        db_err!(conn.execute(
            "UPDATE schema_version SET version = ?1, updated_at = ?2 WHERE id = 1",
            rusqlite::params![schema::SCHEMA_VERSION, now],
        ))?;

        tracing::info!("Database migration complete, now at version {}", current);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_db_open() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("test.db");
        let _db = Database::open(&db_path).unwrap();
    }
}
