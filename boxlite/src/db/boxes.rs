//! Box storage operations using JSON blob pattern.
//!
//! Follows Podman's design:
//! - BoxConfig: Immutable configuration (stored once at creation)
//! - BoxState: Mutable state (updated during lifecycle)
//!
//! Each table has queryable columns for filtering + JSON blob for full struct.

use chrono::Utc;
use rusqlite::{OptionalExtension, params};

use crate::litebox::config::BoxConfig;
use crate::runtime::types::{BoxID, BoxState};
use boxlite_shared::errors::{BoxliteError, BoxliteResult};

use super::{Database, db_err};

/// Box storage wrapping Database.
///
/// Manages BoxConfig (immutable) and BoxState (mutable) tables.
/// Uses JSON blob pattern for flexibility with queryable columns for performance.
#[derive(Clone)]
pub struct BoxStore {
    db: Database,
}

impl BoxStore {
    /// Create a new BoxStore from a Database.
    pub fn new(db: Database) -> Self {
        Self { db }
    }

    // ========================================================================
    // BoxConfig operations (immutable after creation)
    // ========================================================================

    /// Load box configuration by ID.
    #[allow(dead_code)] // API symmetry with load_state
    pub fn load_config(&self, box_id: &str) -> BoxliteResult<Option<BoxConfig>> {
        let conn = self.db.conn();

        let json: Option<String> = db_err!(
            conn.query_row(
                "SELECT json FROM box_config WHERE id = ?1",
                params![box_id],
                |row| row.get(0),
            )
            .optional()
        )?;

        match json {
            Some(j) => {
                let config: BoxConfig = serde_json::from_str(&j).map_err(|e| {
                    BoxliteError::Database(format!("Failed to deserialize config: {}", e))
                })?;
                Ok(Some(config))
            }
            None => Ok(None),
        }
    }

    /// Delete box configuration (and state via CASCADE).
    pub fn delete(&self, box_id: &str) -> BoxliteResult<bool> {
        let conn = self.db.conn();
        let rows_affected =
            db_err!(conn.execute("DELETE FROM box_config WHERE id = ?1", params![box_id],))?;
        Ok(rows_affected > 0)
    }

    // ========================================================================
    // BoxState operations (mutable)
    // ========================================================================

    /// Load box state by ID.
    pub fn load_state(&self, box_id: &str) -> BoxliteResult<Option<BoxState>> {
        let conn = self.db.conn();

        let json: Option<String> = db_err!(
            conn.query_row(
                "SELECT json FROM box_state WHERE id = ?1",
                params![box_id],
                |row| row.get(0),
            )
            .optional()
        )?;

        match json {
            Some(j) => {
                let state: BoxState = serde_json::from_str(&j).map_err(|e| {
                    BoxliteError::Database(format!("Failed to deserialize state: {}", e))
                })?;
                Ok(Some(state))
            }
            None => Ok(None),
        }
    }

    /// Update box state.
    ///
    /// Updates both queryable columns and JSON blob.
    /// Returns error if box doesn't exist (Podman pattern: verify RowsAffected).
    pub fn update_state(&self, box_id: &str, state: &BoxState) -> BoxliteResult<()> {
        let conn = self.db.conn();

        let json = serde_json::to_string(state)
            .map_err(|e| BoxliteError::Database(format!("Failed to serialize state: {}", e)))?;

        let rows_affected = db_err!(conn.execute(
            "UPDATE box_state SET status = ?1, pid = ?2, json = ?3 WHERE id = ?4",
            params![state.status.as_str(), state.pid, json, box_id],
        ))?;

        // Podman pattern: verify rows were actually updated
        if rows_affected == 0 {
            return Err(BoxliteError::NotFound(format!("Box not found: {}", box_id)));
        }

        Ok(())
    }

    // ========================================================================
    // Combined operations
    // ========================================================================

    /// Save both config and initial state atomically.
    ///
    /// Uses a transaction to ensure both inserts succeed or neither does.
    /// Follows Podman pattern of explicit transactions for multi-statement operations.
    pub fn save(&self, config: &BoxConfig, state: &BoxState) -> BoxliteResult<()> {
        let mut conn = self.db.conn();
        let tx = db_err!(conn.transaction())?;

        // Serialize config
        let config_json = serde_json::to_string(config)
            .map_err(|e| BoxliteError::Database(format!("Failed to serialize config: {}", e)))?;

        // Serialize state
        let state_json = serde_json::to_string(state)
            .map_err(|e| BoxliteError::Database(format!("Failed to serialize state: {}", e)))?;

        // Insert config
        db_err!(tx.execute(
            "INSERT INTO box_config (id, created_at, json) VALUES (?1, ?2, ?3)",
            params![config.id, config.created_at.timestamp(), config_json],
        ))?;

        // Insert state
        db_err!(tx.execute(
            "INSERT INTO box_state (id, status, pid, json) VALUES (?1, ?2, ?3, ?4)",
            params![config.id, state.status.as_str(), state.pid, state_json],
        ))?;

        // Commit transaction
        db_err!(tx.commit())?;

        Ok(())
    }

    /// Load both config and state for a box.
    #[allow(dead_code)] // API symmetry - currently unused but part of designed API
    pub fn load(&self, box_id: &str) -> BoxliteResult<Option<(BoxConfig, BoxState)>> {
        let config = self.load_config(box_id)?;
        let state = self.load_state(box_id)?;

        match (config, state) {
            (Some(c), Some(s)) => Ok(Some((c, s))),
            _ => Ok(None),
        }
    }

    /// List all boxes as (config, state) pairs.
    ///
    /// Returns boxes sorted by creation time (newest first).
    pub fn list_all(&self) -> BoxliteResult<Vec<(BoxConfig, BoxState)>> {
        let conn = self.db.conn();

        let mut stmt = db_err!(conn.prepare(
            r#"
            SELECT c.json as config_json, s.json as state_json
            FROM box_config c
            JOIN box_state s ON c.id = s.id
            ORDER BY c.created_at DESC
            "#
        ))?;

        let rows = db_err!(stmt.query_map([], |row| {
            let config_json: String = row.get(0)?;
            let state_json: String = row.get(1)?;
            Ok((config_json, state_json))
        }))?;

        let mut result = Vec::new();
        for row in rows {
            let (config_json, state_json) = db_err!(row)?;
            let config: BoxConfig = serde_json::from_str(&config_json).map_err(|e| {
                BoxliteError::Database(format!("Failed to deserialize config: {}", e))
            })?;
            let state: BoxState = serde_json::from_str(&state_json).map_err(|e| {
                BoxliteError::Database(format!("Failed to deserialize state: {}", e))
            })?;
            result.push((config, state));
        }

        Ok(result)
    }

    /// List active boxes (Starting, Running, Detached).
    pub fn list_active(&self) -> BoxliteResult<Vec<(BoxConfig, BoxState)>> {
        let conn = self.db.conn();

        let mut stmt = db_err!(conn.prepare(
            r#"
            SELECT c.json as config_json, s.json as state_json
            FROM box_config c
            JOIN box_state s ON c.id = s.id
            WHERE s.status IN ('starting', 'running', 'detached')
            ORDER BY c.created_at DESC
            "#
        ))?;

        let rows = db_err!(stmt.query_map([], |row| {
            let config_json: String = row.get(0)?;
            let state_json: String = row.get(1)?;
            Ok((config_json, state_json))
        }))?;

        let mut result = Vec::new();
        for row in rows {
            let (config_json, state_json) = db_err!(row)?;
            let config: BoxConfig = serde_json::from_str(&config_json).map_err(|e| {
                BoxliteError::Database(format!("Failed to deserialize config: {}", e))
            })?;
            let state: BoxState = serde_json::from_str(&state_json).map_err(|e| {
                BoxliteError::Database(format!("Failed to deserialize state: {}", e))
            })?;
            result.push((config, state));
        }

        Ok(result)
    }

    // ========================================================================
    // Reboot detection via alive table
    // ========================================================================

    /// Check if this is a fresh boot (alive record is stale or missing).
    ///
    /// Returns true if reboot detected (need to reset active boxes).
    pub fn check_and_update_boot(&self) -> BoxliteResult<bool> {
        let conn = self.db.conn();
        let current_boot_id = get_boot_id();

        // Check existing alive record
        let existing: Option<String> = db_err!(
            conn.query_row("SELECT boot_id FROM alive WHERE id = 1", [], |row| row
                .get(0))
                .optional()
        )?;

        let is_reboot = match existing {
            None => {
                // First run ever - not a reboot
                false
            }
            Some(stored_boot_id) => {
                // Reboot if boot ID changed
                stored_boot_id != current_boot_id
            }
        };

        // Update alive record
        let now = Utc::now().timestamp();
        db_err!(conn.execute(
            r#"
            INSERT INTO alive (id, boot_id, started_at) VALUES (1, ?1, ?2)
            ON CONFLICT(id) DO UPDATE SET boot_id = ?1, started_at = ?2
            "#,
            params![current_boot_id, now],
        ))?;

        Ok(is_reboot)
    }

    /// Reset all active boxes to stopped state after reboot.
    ///
    /// Called after reboot detection. VM rootfs is preserved, so boxes
    /// become Stopped (not Crashed) and can be restarted.
    pub fn reset_active_boxes_after_reboot(&self) -> BoxliteResult<Vec<BoxID>> {
        let active = self.list_active()?;
        let mut reset_ids = Vec::new();

        for (config, mut state) in active {
            state.reset_for_reboot();
            self.update_state(&config.id, &state)?;
            reset_ids.push(config.id);
        }

        Ok(reset_ids)
    }
}

/// Get system boot ID (unique per boot).
///
/// On macOS: Uses kern.bootsessionuuid
/// On Linux: Uses /proc/sys/kernel/random/boot_id
fn get_boot_id() -> String {
    #[cfg(target_os = "macos")]
    {
        use std::process::Command;
        Command::new("sysctl")
            .args(["-n", "kern.bootsessionuuid"])
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .map(|s| s.trim().to_string())
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string())
    }

    #[cfg(target_os = "linux")]
    {
        std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
            .map(|s| s.trim().to_string())
            .unwrap_or_else(|_| uuid::Uuid::new_v4().to_string())
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        uuid::Uuid::new_v4().to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::litebox::config::ContainerRuntimeConfig;
    use crate::runtime::types::{BoxStatus, ContainerId};
    use crate::vmm::VmmKind;
    use boxlite_shared::Transport;
    use std::path::PathBuf;
    use tempfile::tempdir;

    fn create_test_db() -> (BoxStore, tempfile::TempDir) {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let db = Database::open(&db_path).unwrap();
        (BoxStore::new(db), dir)
    }

    fn create_test_config(id: &str) -> BoxConfig {
        use crate::runtime::options::{BoxOptions, RootfsSpec};
        let now = Utc::now();
        BoxConfig {
            id: id.to_string(),
            name: None,
            created_at: now,
            container: ContainerRuntimeConfig {
                id: ContainerId::new(),
                image: RootfsSpec::Image("test:latest".to_string()),
                image_config: None,
            },
            options: BoxOptions {
                cpus: Some(2),
                memory_mib: Some(512),
                ..Default::default()
            },
            engine_kind: VmmKind::Libkrun,
            transport: Transport::unix(PathBuf::from("/tmp/test.sock")),
            box_home: PathBuf::from("/tmp/boxes/test"),
            ready_socket_path: PathBuf::from("/tmp/ready.sock"),
        }
    }

    #[test]
    fn test_save_and_load_config() {
        let (store, _dir) = create_test_db();
        let config = create_test_config("test1");
        let state = BoxState::new();

        store.save(&config, &state).unwrap();

        let loaded = store.load_config(&config.id).unwrap();
        assert!(loaded.is_some());
        assert_eq!(loaded.unwrap().id, config.id);
    }

    #[test]
    fn test_save_and_load_state() {
        let (store, _dir) = create_test_db();
        let config = create_test_config("test1");
        let state = BoxState::new();

        store.save(&config, &state).unwrap();

        let loaded = store.load_state(&config.id).unwrap();
        assert!(loaded.is_some());
        assert_eq!(loaded.unwrap().status, BoxStatus::Starting);
    }

    #[test]
    fn test_update_state() {
        let (store, _dir) = create_test_db();
        let config = create_test_config("test1");
        let state = BoxState::new();

        store.save(&config, &state).unwrap();

        // Update to running with PID
        let mut new_state = state.clone();
        new_state.set_status(BoxStatus::Running);
        new_state.set_pid(Some(12345));
        store.update_state(&config.id, &new_state).unwrap();

        let loaded = store.load_state(&config.id).unwrap().unwrap();
        assert_eq!(loaded.status, BoxStatus::Running);
        assert_eq!(loaded.pid, Some(12345));
    }

    #[test]
    fn test_delete() {
        let (store, _dir) = create_test_db();
        let config = create_test_config("test1");
        let state = BoxState::new();

        store.save(&config, &state).unwrap();
        assert!(store.load(&config.id).unwrap().is_some());

        store.delete(&config.id).unwrap();
        assert!(store.load(&config.id).unwrap().is_none());
    }

    #[test]
    fn test_list_all() {
        let (store, _dir) = create_test_db();

        // Create multiple boxes
        for i in 0..3 {
            let config = create_test_config(&format!("test{}", i));
            let state = BoxState::new();
            store.save(&config, &state).unwrap();
        }

        let all = store.list_all().unwrap();
        assert_eq!(all.len(), 3);
    }

    #[test]
    fn test_list_active() {
        let (store, _dir) = create_test_db();

        // Create running box
        let config1 = create_test_config("running1");
        let mut state1 = BoxState::new();
        state1.set_status(BoxStatus::Running);
        store.save(&config1, &state1).unwrap();

        // Create stopped box
        let config2 = create_test_config("stopped1");
        let mut state2 = BoxState::new();
        state2.set_status(BoxStatus::Stopped);
        store.save(&config2, &state2).unwrap();

        let active = store.list_active().unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].0.id, "running1");
    }

    #[test]
    fn test_reboot_detection() {
        let (store, _dir) = create_test_db();

        // First call - not a reboot
        let is_reboot = store.check_and_update_boot().unwrap();
        assert!(!is_reboot);

        // Second call with same boot_id - still not a reboot
        let is_reboot = store.check_and_update_boot().unwrap();
        assert!(!is_reboot);
    }

    #[test]
    fn test_reset_active_boxes_after_reboot() {
        let (store, _dir) = create_test_db();

        // Create running box
        let config = create_test_config("test1");
        let mut state = BoxState::new();
        state.set_status(BoxStatus::Running);
        state.set_pid(Some(12345));
        store.save(&config, &state).unwrap();

        // Reset active boxes after reboot
        let reset_ids = store.reset_active_boxes_after_reboot().unwrap();
        assert_eq!(reset_ids.len(), 1);
        assert_eq!(reset_ids[0], "test1");

        // Verify state changed to Stopped (not Crashed - rootfs preserved)
        let loaded = store.load_state(&config.id).unwrap().unwrap();
        assert_eq!(loaded.status, BoxStatus::Stopped);
        assert_eq!(loaded.pid, None);
    }
}
