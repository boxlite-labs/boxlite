//! Box state backend.
//!
//! Pure database access layer for box persistence.
//! No in-memory cache - queries go directly to database.

use std::sync::Arc;

use boxlite_shared::errors::{BoxliteError, BoxliteResult};

use crate::db::BoxStore;
use crate::litebox::config::BoxConfig;
use crate::runtime::types::{BoxID, BoxState};

/// State backend for box persistence.
///
/// Pure database access layer for box state.
/// All queries go directly to database - no in-memory cache.
#[derive(Clone)]
pub struct BoxManager {
    store: Arc<BoxStore>,
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
            store: Arc::new(store),
        }
    }

    // ========================================================================
    // State Interface
    // ========================================================================

    /// Add a new box to the database.
    pub fn add_box(&self, config: &BoxConfig, state: &BoxState) -> BoxliteResult<()> {
        // Check name uniqueness if name is set
        if let Some(ref name) = config.name
            && self.lookup_box_id(name)?.is_some()
        {
            return Err(BoxliteError::InvalidState(format!(
                "box with name '{}' already exists",
                name
            )));
        }

        // Check ID uniqueness
        if self.has_box(&config.id)? {
            return Err(BoxliteError::InvalidState(format!(
                "box {} already exists",
                config.id
            )));
        }

        self.store.save(config, state)?;

        tracing::debug!(
            box_id = %config.id,
            name = ?config.name,
            status = ?state.status,
            "Added box to state"
        );

        Ok(())
    }

    /// Remove a box from the database.
    pub fn remove_box(&self, id: &BoxID) -> BoxliteResult<()> {
        // Check if box exists
        if !self.has_box(id)? {
            return Err(BoxliteError::NotFound(format!("box {}", id)));
        }

        self.store.delete(id)?;

        tracing::debug!(box_id = %id, "Removed box from state");

        Ok(())
    }

    /// Get a box by exact ID.
    pub fn box_by_id(&self, id: &BoxID) -> BoxliteResult<Option<(BoxConfig, BoxState)>> {
        self.store.load(id)
    }

    /// Lookup a box by ID prefix or name.
    ///
    /// Tries exact name match first, then ID prefix match.
    pub fn lookup_box(&self, id_or_name: &str) -> BoxliteResult<Option<(BoxConfig, BoxState)>> {
        // First try exact ID match
        if let Some(result) = self.store.load(id_or_name)? {
            return Ok(Some(result));
        }

        // Try name match
        let all = self.store.list_all()?;

        // Exact name match
        for (config, state) in &all {
            if config.name.as_deref() == Some(id_or_name) {
                return Ok(Some((config.clone(), state.clone())));
            }
        }

        // ID prefix match
        let matches: Vec<_> = all
            .iter()
            .filter(|(config, _)| config.id.starts_with(id_or_name))
            .collect();

        match matches.len() {
            0 => Ok(None),
            1 => Ok(Some((matches[0].0.clone(), matches[0].1.clone()))),
            _ => Err(BoxliteError::InvalidArgument(format!(
                "multiple boxes match prefix '{}': {:?}",
                id_or_name,
                matches
                    .iter()
                    .map(|(c, _)| c.id.as_str())
                    .collect::<Vec<_>>()
            ))),
        }
    }

    /// Lookup a box ID by ID prefix or name.
    pub fn lookup_box_id(&self, id_or_name: &str) -> BoxliteResult<Option<BoxID>> {
        self.lookup_box(id_or_name)
            .map(|opt| opt.map(|(config, _)| config.id))
    }

    /// Check if a box exists by exact ID.
    pub fn has_box(&self, id: &BoxID) -> BoxliteResult<bool> {
        self.store.load(id).map(|opt| opt.is_some())
    }

    /// Get all boxes.
    pub fn all_boxes(&self, _load_state: bool) -> BoxliteResult<Vec<(BoxConfig, BoxState)>> {
        self.store.list_all()
    }

    /// Save box state to the database.
    ///
    /// Reads state from the provided BoxState and persists to DB.
    pub fn save_box(&self, id: &BoxID, state: &BoxState) -> BoxliteResult<()> {
        self.store.update_state(id, state)?;

        tracing::trace!(
            box_id = %id,
            status = ?state.status,
            "Saved box state to database"
        );

        Ok(())
    }

    /// Load box state from the database.
    ///
    /// Returns the latest state from DB.
    pub fn update_box(&self, id: &BoxID) -> BoxliteResult<BoxState> {
        self.store
            .load_state(id)?
            .ok_or_else(|| BoxliteError::NotFound(format!("box {} state not found", id)))
    }

    // ========================================================================
    // Recovery helpers
    // ========================================================================

    /// Check and handle system reboot.
    ///
    /// Returns true if a reboot was detected.
    pub fn check_and_handle_reboot(&self) -> BoxliteResult<bool> {
        let is_reboot = self.store.check_and_update_boot()?;

        if is_reboot {
            tracing::info!("Detected system reboot, resetting active boxes to stopped");
            let reset_ids = self.store.reset_active_boxes_after_reboot()?;
            for id in &reset_ids {
                tracing::info!(box_id = %id, "Reset box to stopped after reboot");
            }
        }

        Ok(is_reboot)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Database;
    use crate::litebox::config::ContainerRuntimeConfig;
    use crate::runtime::types::{BoxStatus, ContainerId};
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
        BoxConfig {
            id: id.to_string(),
            name: None,
            created_at: Utc::now(),
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
            box_home: PathBuf::from("/tmp/box"),
            ready_socket_path: PathBuf::from("/tmp/ready"),
        }
    }

    fn create_test_state(status: BoxStatus) -> BoxState {
        let mut state = BoxState::new();
        state.set_status(status);
        state
    }

    #[test]
    fn test_add_box_and_box_by_id() {
        let store = create_test_store();
        let manager = BoxManager::new(store);
        let config = create_test_config("test-id");
        let state = BoxState::new();

        manager.add_box(&config, &state).unwrap();

        let (retrieved_config, retrieved_state) = manager.box_by_id(&config.id).unwrap().unwrap();
        assert_eq!(retrieved_config.id, config.id);
        assert_eq!(retrieved_state.status, BoxStatus::Starting);
    }

    #[test]
    fn test_add_box_duplicate_id_fails() {
        let store = create_test_store();
        let manager = BoxManager::new(store);
        let config = create_test_config("test-id");
        let state = BoxState::new();

        manager.add_box(&config, &state).unwrap();
        let result = manager.add_box(&config, &state);

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("already exists"));
    }

    #[test]
    fn test_add_box_duplicate_name_fails() {
        let store = create_test_store();
        let manager = BoxManager::new(store);

        let mut config1 = create_test_config("test-id-1");
        config1.name = Some("my-box".to_string());
        let state1 = BoxState::new();

        let mut config2 = create_test_config("test-id-2");
        config2.name = Some("my-box".to_string());
        let state2 = BoxState::new();

        manager.add_box(&config1, &state1).unwrap();
        let result = manager.add_box(&config2, &state2);

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("already exists"));
    }

    #[test]
    fn test_has_box() {
        let store = create_test_store();
        let manager = BoxManager::new(store);
        let config = create_test_config("test-id");
        let state = BoxState::new();

        assert!(!manager.has_box(&config.id).unwrap());
        manager.add_box(&config, &state).unwrap();
        assert!(manager.has_box(&config.id).unwrap());
    }

    #[test]
    fn test_lookup_box_by_name() {
        let store = create_test_store();
        let manager = BoxManager::new(store);

        let mut config = create_test_config("test-id-12345");
        config.name = Some("my-box".to_string());
        let state = BoxState::new();

        manager.add_box(&config, &state).unwrap();

        let result = manager.lookup_box("my-box").unwrap();
        assert!(result.is_some());
        assert_eq!(result.unwrap().0.id, "test-id-12345");
    }

    #[test]
    fn test_lookup_box_by_id_prefix() {
        let store = create_test_store();
        let manager = BoxManager::new(store);
        let config = create_test_config("test-id-12345");
        let state = BoxState::new();

        manager.add_box(&config, &state).unwrap();

        let result = manager.lookup_box("test-id-123").unwrap();
        assert!(result.is_some());
        assert_eq!(result.unwrap().0.id, "test-id-12345");
    }

    #[test]
    fn test_lookup_box_ambiguous_prefix() {
        let store = create_test_store();
        let manager = BoxManager::new(store);

        manager
            .add_box(&create_test_config("test-id-1"), &BoxState::new())
            .unwrap();
        manager
            .add_box(&create_test_config("test-id-2"), &BoxState::new())
            .unwrap();

        let result = manager.lookup_box("test-id");
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("multiple boxes match")
        );
    }

    #[test]
    fn test_all_boxes() {
        let store = create_test_store();
        let manager = BoxManager::new(store);

        manager
            .add_box(
                &create_test_config("id1"),
                &create_test_state(BoxStatus::Running),
            )
            .unwrap();
        manager
            .add_box(
                &create_test_config("id2"),
                &create_test_state(BoxStatus::Stopped),
            )
            .unwrap();
        manager
            .add_box(
                &create_test_config("id3"),
                &create_test_state(BoxStatus::Running),
            )
            .unwrap();

        let boxes = manager.all_boxes(true).unwrap();
        assert_eq!(boxes.len(), 3);
    }

    #[test]
    fn test_remove_box() {
        let store = create_test_store();
        let manager = BoxManager::new(store);
        let config = create_test_config("test-id");
        let state = BoxState::new();

        manager.add_box(&config, &state).unwrap();
        manager.remove_box(&config.id).unwrap();

        assert!(manager.box_by_id(&config.id).unwrap().is_none());
    }

    #[test]
    fn test_save_and_update_box() {
        let store = create_test_store();
        let manager = BoxManager::new(store);
        let config = create_test_config("test-id");
        let state = BoxState::new();

        manager.add_box(&config, &state).unwrap();

        // Save new state
        let mut new_state = BoxState::new();
        new_state.set_status(BoxStatus::Running);
        new_state.set_pid(Some(12345));
        manager.save_box(&config.id, &new_state).unwrap();

        // Update (load from DB)
        let loaded_state = manager.update_box(&config.id).unwrap();
        assert_eq!(loaded_state.status, BoxStatus::Running);
        assert_eq!(loaded_state.pid, Some(12345));
    }
}
