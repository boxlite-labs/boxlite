//! Image index storage operations.
//!
//! Provides database-backed storage for the image index, replacing the
//! JSON file-based approach for better reliability and concurrent access.

use rusqlite::{OptionalExtension, params};

use boxlite_shared::errors::{BoxliteError, BoxliteResult};

use super::{Database, db_err};

/// Metadata for a cached image.
#[derive(Debug, Clone)]
pub struct CachedImage {
    /// Manifest digest of the final image (sha256:...)
    /// For multi-platform images, this is the platform-specific manifest digest
    pub manifest_digest: String,

    /// Config blob digest (sha256:...)
    pub config_digest: String,

    /// Layer digests in order
    pub layers: Vec<String>,

    /// When the image was cached (ISO 8601)
    pub cached_at: String,

    /// Whether all layers are fully downloaded
    pub complete: bool,
}

/// Image index storage wrapping Database.
///
/// Manages image index entries in the database.
#[derive(Clone)]
pub struct ImageIndexStore {
    db: Database,
}

impl ImageIndexStore {
    /// Create a new ImageIndexStore from a Database.
    pub fn new(db: Database) -> Self {
        Self { db }
    }

    /// Get cached image by reference.
    ///
    /// Returns None if image not in index.
    pub fn get(&self, reference: &str) -> BoxliteResult<Option<CachedImage>> {
        let conn = self.db.conn();

        let row: Option<(String, String, String, String, i32)> = db_err!(
            conn.query_row(
                "SELECT manifest_digest, config_digest, layers, cached_at, complete FROM image_index WHERE reference = ?1",
                params![reference],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
            )
            .optional()
        )?;

        match row {
            Some((manifest_digest, config_digest, layers_json, cached_at, complete)) => {
                let layers: Vec<String> = serde_json::from_str(&layers_json).map_err(|e| {
                    BoxliteError::Database(format!("Failed to deserialize layers: {}", e))
                })?;
                Ok(Some(CachedImage {
                    manifest_digest,
                    config_digest,
                    layers,
                    cached_at,
                    complete: complete != 0,
                }))
            }
            None => Ok(None),
        }
    }

    /// Add or update cached image.
    pub fn upsert(&self, reference: &str, image: &CachedImage) -> BoxliteResult<()> {
        let conn = self.db.conn();

        let layers_json = serde_json::to_string(&image.layers)
            .map_err(|e| BoxliteError::Database(format!("Failed to serialize layers: {}", e)))?;

        db_err!(conn.execute(
            r#"
            INSERT INTO image_index (reference, manifest_digest, config_digest, layers, cached_at, complete)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6)
            ON CONFLICT(reference) DO UPDATE SET
                manifest_digest = excluded.manifest_digest,
                config_digest = excluded.config_digest,
                layers = excluded.layers,
                cached_at = excluded.cached_at,
                complete = excluded.complete
            "#,
            params![
                reference,
                image.manifest_digest,
                image.config_digest,
                layers_json,
                image.cached_at,
                if image.complete { 1 } else { 0 }
            ],
        ))?;

        Ok(())
    }

    /// Remove cached image from index.
    #[allow(dead_code)]
    pub fn remove(&self, reference: &str) -> BoxliteResult<bool> {
        let conn = self.db.conn();
        let rows_affected = db_err!(conn.execute(
            "DELETE FROM image_index WHERE reference = ?1",
            params![reference]
        ))?;
        Ok(rows_affected > 0)
    }

    /// Get number of cached images in index.
    pub fn len(&self) -> BoxliteResult<usize> {
        let conn = self.db.conn();
        let count: i64 =
            db_err!(conn.query_row("SELECT COUNT(*) FROM image_index", [], |row| row.get(0)))?;
        Ok(count as usize)
    }

    /// Check if index is empty.
    #[allow(dead_code)]
    pub fn is_empty(&self) -> BoxliteResult<bool> {
        Ok(self.len()? == 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn create_test_db() -> (ImageIndexStore, TempDir) {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("test.db");
        let db = Database::open(&db_path).unwrap();
        (ImageIndexStore::new(db), dir)
    }

    #[test]
    fn test_upsert_and_get() {
        let (store, _dir) = create_test_db();

        let image = CachedImage {
            manifest_digest: "sha256:abc123".to_string(),
            config_digest: "sha256:config123".to_string(),
            layers: vec!["sha256:layer1".to_string(), "sha256:layer2".to_string()],
            cached_at: "2025-10-24T12:00:00Z".to_string(),
            complete: true,
        };

        store.upsert("python:alpine", &image).unwrap();

        let loaded = store.get("python:alpine").unwrap();
        assert!(loaded.is_some());
        let loaded = loaded.unwrap();
        assert_eq!(loaded.manifest_digest, "sha256:abc123");
        assert_eq!(loaded.config_digest, "sha256:config123");
        assert_eq!(loaded.layers.len(), 2);
        assert!(loaded.complete);
    }

    #[test]
    fn test_upsert_updates_existing() {
        let (store, _dir) = create_test_db();

        let image1 = CachedImage {
            manifest_digest: "sha256:abc123".to_string(),
            config_digest: "sha256:config123".to_string(),
            layers: vec!["sha256:layer1".to_string()],
            cached_at: "2025-10-24T12:00:00Z".to_string(),
            complete: true,
        };

        store.upsert("python:alpine", &image1).unwrap();

        let image2 = CachedImage {
            manifest_digest: "sha256:def456".to_string(),
            config_digest: "sha256:config456".to_string(),
            layers: vec!["sha256:layer2".to_string()],
            cached_at: "2025-10-25T12:00:00Z".to_string(),
            complete: false,
        };

        store.upsert("python:alpine", &image2).unwrap();

        let loaded = store.get("python:alpine").unwrap().unwrap();
        assert_eq!(loaded.manifest_digest, "sha256:def456");
        assert!(!loaded.complete);

        assert_eq!(store.len().unwrap(), 1);
    }

    #[test]
    fn test_get_nonexistent() {
        let (store, _dir) = create_test_db();
        let loaded = store.get("nonexistent:tag").unwrap();
        assert!(loaded.is_none());
    }

    #[test]
    fn test_remove() {
        let (store, _dir) = create_test_db();

        let image = CachedImage {
            manifest_digest: "sha256:abc123".to_string(),
            config_digest: "sha256:config123".to_string(),
            layers: vec![],
            cached_at: "2025-10-24T12:00:00Z".to_string(),
            complete: true,
        };

        store.upsert("python:alpine", &image).unwrap();
        assert_eq!(store.len().unwrap(), 1);

        let removed = store.remove("python:alpine").unwrap();
        assert!(removed);
        assert_eq!(store.len().unwrap(), 0);

        let removed_again = store.remove("python:alpine").unwrap();
        assert!(!removed_again);
    }

    #[test]
    fn test_len_and_is_empty() {
        let (store, _dir) = create_test_db();

        assert!(store.is_empty().unwrap());
        assert_eq!(store.len().unwrap(), 0);

        let image = CachedImage {
            manifest_digest: "sha256:abc123".to_string(),
            config_digest: "sha256:config123".to_string(),
            layers: vec![],
            cached_at: "2025-10-24T12:00:00Z".to_string(),
            complete: true,
        };

        store.upsert("python:alpine", &image).unwrap();
        assert!(!store.is_empty().unwrap());
        assert_eq!(store.len().unwrap(), 1);
    }
}
