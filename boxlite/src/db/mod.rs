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

    fn init_schema(conn: &Connection) -> BoxliteResult<()> {
        for sql in schema::all_schemas() {
            db_err!(conn.execute_batch(sql))?;
        }

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
                let now = Utc::now().to_rfc3339();
                db_err!(conn.execute(
                    "INSERT INTO schema_version (id, version, updated_at) VALUES (1, ?1, ?2)",
                    rusqlite::params![schema::SCHEMA_VERSION, now],
                ))?;
                tracing::info!(
                    "Initialized database schema version {}",
                    schema::SCHEMA_VERSION
                );
            }
            Some(v) if v < schema::SCHEMA_VERSION => {
                tracing::warn!(
                    "Database schema version {} is older than current {}. Migrations not yet implemented.",
                    v,
                    schema::SCHEMA_VERSION
                );
            }
            Some(v) if v > schema::SCHEMA_VERSION => {
                return Err(BoxliteError::Database(format!(
                    "Database schema version {} is newer than supported {}. Please upgrade boxlite.",
                    v,
                    schema::SCHEMA_VERSION
                )));
            }
            Some(_) => {}
        }

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
