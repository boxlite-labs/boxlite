//! Thread-safe box manager implementation.
//!
//! Uses Podman-style separation of BoxConfig (immutable) and BoxState (mutable).

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use boxlite_shared::errors::{BoxliteError, BoxliteResult};

use crate::db::BoxStore;
use crate::litebox::config::BoxConfig;
use crate::runtime::types::{BoxID, BoxInfo, BoxState, BoxStatus};

/// In-memory cache entry combining config and state.
#[derive(Debug, Clone)]
struct CacheEntry {
    config: BoxConfig,
    state: BoxState,
}

/// Thread-safe manager for tracking live boxes.
///
/// Owns both in-memory cache and database persistence layer.
/// All mutations follow database-first pattern internally.
///
/// # Design
///
/// - **Shared ownership**: Cloneable via `Arc`, passed to runtime and handles
/// - **Concurrent access**: RwLock allows multiple readers, single writer
/// - **Database-first**: All state changes persist to DB before updating cache
/// - **Config/State separation**: Follows Podman's pattern for immutable config vs mutable state
#[derive(Clone)]
pub struct BoxManager {
    inner: Arc<RwLock<BoxManagerInner>>,
}

struct BoxManagerInner {
    boxes: HashMap<BoxID, CacheEntry>,
    store: BoxStore,
}

impl std::fmt::Debug for BoxManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BoxManager").finish()
    }
}

impl BoxManager {
    /// Create a new manager with the given store.
    pub fn new(store: BoxStore) -> Self {
        Self {
            inner: Arc::new(RwLock::new(BoxManagerInner {
                boxes: HashMap::new(),
                store,
            })),
        }
    }

    /// Register a new box.
    ///
    /// Database-first: saves to DB before caching in memory.
    ///
    /// # Errors
    ///
    /// Returns error if a box with this ID already exists or DB write fails.
    pub fn register(&self, config: BoxConfig, state: BoxState) -> BoxliteResult<()> {
        let mut inner = self
            .inner
            .write()
            .map_err(|e| BoxliteError::Internal(format!("manager lock poisoned: {}", e)))?;

        if inner.boxes.contains_key(&config.id) {
            return Err(BoxliteError::Internal(format!(
                "box {} already registered",
                config.id
            )));
        }

        // Database-first: persist before caching
        inner.store.save(&config, &state)?;

        tracing::debug!(
            box_id = %config.id,
            status = ?state.status,
            "Registering box"
        );

        let id = config.id.clone();
        inner.boxes.insert(id, CacheEntry { config, state });
        Ok(())
    }

    /// Register a box from recovery (already persisted in DB).
    ///
    /// Used during startup to load boxes from database into memory cache.
    pub fn register_recovered(&self, config: BoxConfig, state: BoxState) -> BoxliteResult<()> {
        let mut inner = self
            .inner
            .write()
            .map_err(|e| BoxliteError::Internal(format!("manager lock poisoned: {}", e)))?;

        if inner.boxes.contains_key(&config.id) {
            return Err(BoxliteError::Internal(format!(
                "box {} already registered",
                config.id
            )));
        }

        tracing::debug!(
            box_id = %config.id,
            status = ?state.status,
            "Registering recovered box"
        );

        let id = config.id.clone();
        inner.boxes.insert(id, CacheEntry { config, state });
        Ok(())
    }

    /// Update the status of an existing box.
    ///
    /// Database-first: updates DB before updating cache.
    pub fn update_status(&self, id: &BoxID, new_status: BoxStatus) -> BoxliteResult<()> {
        let mut inner = self
            .inner
            .write()
            .map_err(|e| BoxliteError::Internal(format!("manager lock poisoned: {}", e)))?;

        if !inner.boxes.contains_key(id) {
            return Err(BoxliteError::Internal(format!("box {} not found", id)));
        }

        // Database-first
        inner.store.update_status(id, new_status)?;

        // Now update cache
        if let Some(entry) = inner.boxes.get_mut(id) {
            tracing::debug!(
                box_id = %id,
                old_status = ?entry.state.status,
                new_status = ?new_status,
                "Updating box status"
            );
            entry.state.set_status(new_status);
        }
        Ok(())
    }

    /// Update the PID of an existing box.
    ///
    /// Database-first: updates DB before updating cache.
    pub fn update_pid(&self, id: &BoxID, pid: Option<u32>) -> BoxliteResult<()> {
        let mut inner = self
            .inner
            .write()
            .map_err(|e| BoxliteError::Internal(format!("manager lock poisoned: {}", e)))?;

        if !inner.boxes.contains_key(id) {
            return Err(BoxliteError::Internal(format!("box {} not found", id)));
        }

        // Database-first
        inner.store.update_pid(id, pid)?;

        // Now update cache
        if let Some(entry) = inner.boxes.get_mut(id) {
            tracing::trace!(box_id = %id, pid = ?pid, "Updating box PID");
            entry.state.set_pid(pid);
        }
        Ok(())
    }

    /// Update the container ID of an existing box.
    ///
    /// Database-first: updates DB before updating cache.
    /// Container ID is assigned after successful initialization.
    pub fn update_container_id(&self, id: &BoxID, container_id: String) -> BoxliteResult<()> {
        let mut inner = self
            .inner
            .write()
            .map_err(|e| BoxliteError::Internal(format!("manager lock poisoned: {}", e)))?;

        if !inner.boxes.contains_key(id) {
            return Err(BoxliteError::Internal(format!("box {} not found", id)));
        }

        // Database-first
        inner.store.update_container_id(id, &container_id)?;

        // Now update cache
        if let Some(entry) = inner.boxes.get_mut(id) {
            tracing::trace!(box_id = %id, container_id = %container_id, "Updating box container_id");
            let container_id = crate::runtime::types::ContainerId::parse(&container_id)
                .ok_or_else(|| {
                    BoxliteError::Internal(format!("Invalid container ID format: {}", container_id))
                })?;
            entry.state.container_id = Some(container_id);
        }
        Ok(())
    }

    /// Get config and state for a specific box.
    ///
    /// Returns `Ok(None)` if the box doesn't exist.
    pub fn get(&self, id: &BoxID) -> BoxliteResult<Option<(BoxConfig, BoxState)>> {
        let inner = self
            .inner
            .read()
            .map_err(|e| BoxliteError::Internal(format!("manager lock poisoned: {}", e)))?;

        Ok(inner
            .boxes
            .get(id)
            .map(|e| (e.config.clone(), e.state.clone())))
    }

    /// Get info for a specific box.
    pub fn get_info(&self, id: &BoxID) -> BoxliteResult<Option<BoxInfo>> {
        self.get(id)
            .map(|opt| opt.map(|(config, state)| BoxInfo::new(&config, &state)))
    }

    /// Get config and state for a box by name.
    ///
    /// Returns `Ok(None)` if no box with that name exists.
    pub fn get_by_name(&self, name: &str) -> BoxliteResult<Option<(BoxConfig, BoxState)>> {
        let inner = self
            .inner
            .read()
            .map_err(|e| BoxliteError::Internal(format!("manager lock poisoned: {}", e)))?;

        Ok(inner
            .boxes
            .values()
            .find(|e| e.config.name.as_deref() == Some(name))
            .map(|e| (e.config.clone(), e.state.clone())))
    }

    /// List all boxes, sorted by creation time (newest first).
    pub fn list(&self) -> BoxliteResult<Vec<BoxInfo>> {
        let inner = self
            .inner
            .read()
            .map_err(|e| BoxliteError::Internal(format!("manager lock poisoned: {}", e)))?;

        let mut infos: Vec<BoxInfo> = inner
            .boxes
            .values()
            .map(|e| BoxInfo::new(&e.config, &e.state))
            .collect();

        // Sort by creation time (newest first)
        infos.sort_by(|a, b| b.created_at.cmp(&a.created_at));

        Ok(infos)
    }

    /// Remove a box from the manager.
    ///
    /// Database-first: deletes from DB before removing from cache.
    ///
    /// # Errors
    ///
    /// Returns error if:
    /// - Box doesn't exist
    /// - Box is still in an active state (Starting, Running, Detached)
    /// - DB delete fails
    pub fn remove(&self, id: &BoxID) -> BoxliteResult<(BoxConfig, BoxState)> {
        let mut inner = self
            .inner
            .write()
            .map_err(|e| BoxliteError::Internal(format!("manager lock poisoned: {}", e)))?;

        // Check if box exists and is in terminal state
        if let Some(entry) = inner.boxes.get(id) {
            if entry.state.status.is_active() {
                return Err(BoxliteError::Internal(format!(
                    "cannot remove active box {} (status: {:?})",
                    id, entry.state.status
                )));
            }
        } else {
            return Err(BoxliteError::Internal(format!("box {} not found", id)));
        }

        // Database-first
        inner.store.delete(id)?;

        tracing::debug!(box_id = %id, "Removing box from manager");
        let entry = inner
            .boxes
            .remove(id)
            .ok_or_else(|| BoxliteError::Internal(format!("box {} not found", id)))?;
        Ok((entry.config, entry.state))
    }

    /// Check process liveness and update states accordingly.
    ///
    /// Uses `kill(pid, 0)` to check if process exists without sending signal.
    /// Returns IDs of newly crashed boxes.
    ///
    /// Designed for periodic health monitoring but not yet exposed in public API.
    #[allow(dead_code)]
    pub fn refresh_states(&self) -> BoxliteResult<Vec<BoxID>> {
        let mut inner = self
            .inner
            .write()
            .map_err(|e| BoxliteError::Internal(format!("manager lock poisoned: {}", e)))?;

        let mut newly_crashed = Vec::new();

        for (id, entry) in inner.boxes.iter_mut() {
            // Only check active boxes
            if !entry.state.status.is_active() {
                continue;
            }

            if let Some(pid) = entry.state.pid {
                // Check if process exists using kill(pid, 0)
                let alive = crate::util::is_process_alive(pid);

                if !alive {
                    tracing::warn!(
                        box_id = %id,
                        pid = pid,
                        old_status = ?entry.state.status,
                        "Detected crashed box process, marking as Crashed"
                    );
                    entry.state.mark_crashed();
                    newly_crashed.push(id.clone());
                }
            }
        }

        // Persist crashed state to DB
        for id in &newly_crashed {
            if let Err(e) = inner.store.mark_crashed(id) {
                tracing::warn!("Failed to persist crashed state for box {}: {}", id, e);
            }
        }

        Ok(newly_crashed)
    }

    /// Mark a box as crashed.
    ///
    /// Database-first: updates DB before updating cache.
    pub fn mark_crashed(&self, id: &BoxID) -> BoxliteResult<()> {
        let mut inner = self
            .inner
            .write()
            .map_err(|e| BoxliteError::Internal(format!("manager lock poisoned: {}", e)))?;

        // Database-first
        inner.store.mark_crashed(id)?;

        if let Some(entry) = inner.boxes.get_mut(id) {
            entry.state.mark_crashed();
        }

        Ok(())
    }

    /// Load all persisted boxes from database.
    ///
    /// Used during recovery to get the list of boxes to restore.
    pub fn load_all_persisted(&self) -> BoxliteResult<Vec<(BoxConfig, BoxState)>> {
        let inner = self
            .inner
            .read()
            .map_err(|e| BoxliteError::Internal(format!("manager lock poisoned: {}", e)))?;

        inner.store.list_all()
    }

    /// Check and handle system reboot.
    ///
    /// Returns true if a reboot was detected.
    pub fn check_and_handle_reboot(&self) -> BoxliteResult<bool> {
        let inner = self
            .inner
            .read()
            .map_err(|e| BoxliteError::Internal(format!("manager lock poisoned: {}", e)))?;

        let is_reboot = inner.store.check_and_update_boot()?;

        if is_reboot {
            tracing::info!("Detected system reboot, resetting active boxes to stopped");
            let reset_ids = inner.store.reset_active_boxes_after_reboot()?;
            for id in &reset_ids {
                tracing::info!(box_id = %id, "Reset box to stopped after reboot (rootfs preserved)");
            }
        }

        Ok(is_reboot)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Database;
    use crate::vmm::VmmKind;
    use boxlite_shared::Transport;
    use chrono::Utc;
    use std::path::PathBuf;
    use tempfile::tempdir;

    fn create_test_store() -> BoxStore {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let db = Database::open(&db_path).unwrap();
        BoxStore::new(db)
    }

    fn create_test_config(id: &str) -> BoxConfig {
        use crate::runtime::options::{BoxOptions, RootfsSpec};
        let now = Utc::now();
        BoxConfig {
            id: id.to_string(),
            created_at: now,
            options: BoxOptions {
                cpus: Some(2),
                memory_mib: Some(512),
                rootfs: RootfsSpec::Image("test:latest".to_string()),
                ..Default::default()
            },
            engine_kind: VmmKind::Libkrun,
            transport: Transport::unix(PathBuf::from("/tmp/test.sock")),
            box_home: PathBuf::from("/tmp/box"),
            ready_socket_path: PathBuf::from("/tmp/ready"),
        }
    }

    fn create_test_state(status: BoxStatus) -> BoxState {
        let mut state = BoxState::new();
        state.set_status(status);
        state.set_pid(Some(99999));
        state
    }

    #[test]
    fn test_register_and_get() {
        let store = create_test_store();
        let manager = BoxManager::new(store);
        let config = create_test_config("test-id");
        let state = BoxState::new();

        manager.register(config.clone(), state.clone()).unwrap();

        let (retrieved_config, retrieved_state) = manager.get(&config.id).unwrap().unwrap();
        assert_eq!(retrieved_config.id, config.id);
        assert_eq!(retrieved_state.status, BoxStatus::Starting);
    }

    #[test]
    fn test_duplicate_registration_fails() {
        let store = create_test_store();
        let manager = BoxManager::new(store);
        let config = create_test_config("test-id");
        let state = BoxState::new();

        manager.register(config.clone(), state.clone()).unwrap();
        let result = manager.register(config, state);

        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("already registered")
        );
    }

    #[test]
    fn test_update_status() {
        let store = create_test_store();
        let manager = BoxManager::new(store);
        let config = create_test_config("test-id");
        let state = BoxState::new();

        manager.register(config.clone(), state).unwrap();
        manager
            .update_status(&config.id, BoxStatus::Running)
            .unwrap();

        let (_, retrieved_state) = manager.get(&config.id).unwrap().unwrap();
        assert_eq!(retrieved_state.status, BoxStatus::Running);
    }

    #[test]
    fn test_update_pid() {
        let store = create_test_store();
        let manager = BoxManager::new(store);
        let config = create_test_config("test-id");
        let state = BoxState::new();

        manager.register(config.clone(), state).unwrap();
        manager.update_pid(&config.id, Some(12345)).unwrap();

        let (_, retrieved_state) = manager.get(&config.id).unwrap().unwrap();
        assert_eq!(retrieved_state.pid, Some(12345));
    }

    #[test]
    fn test_list_boxes() {
        let store = create_test_store();
        let manager = BoxManager::new(store);

        manager
            .register(
                create_test_config("id1"),
                create_test_state(BoxStatus::Running),
            )
            .unwrap();
        manager
            .register(
                create_test_config("id2"),
                create_test_state(BoxStatus::Stopped),
            )
            .unwrap();
        manager
            .register(
                create_test_config("id3"),
                create_test_state(BoxStatus::Running),
            )
            .unwrap();

        let boxes = manager.list().unwrap();
        assert_eq!(boxes.len(), 3);
    }

    #[test]
    fn test_remove_stopped_box() {
        let store = create_test_store();
        let manager = BoxManager::new(store);
        let config = create_test_config("test-id");
        let state = create_test_state(BoxStatus::Stopped);

        manager.register(config.clone(), state).unwrap();
        manager.remove(&config.id).unwrap();

        assert!(manager.get(&config.id).unwrap().is_none());
    }

    #[test]
    fn test_cannot_remove_running_box() {
        let store = create_test_store();
        let manager = BoxManager::new(store);
        let config = create_test_config("test-id");
        let state = create_test_state(BoxStatus::Running);

        manager.register(config.clone(), state).unwrap();
        let result = manager.remove(&config.id);

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("active box"));
    }
}
