//! Box lifecycle status and state machine.
//!
//! Defines the possible states of a box and valid transitions between them.

use crate::ContainerId;
use boxlite_shared::errors::{BoxliteError, BoxliteResult};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Lifecycle status of a box.
///
/// Represents the current operational state of a VM box.
/// Transitions between states are validated by the state machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BoxStatus {
    /// Cannot determine box state (error recovery).
    Unknown,

    /// Box is registered, initialization in progress.
    /// VM process may or may not be spawned (lazy init).
    Starting,

    /// Box is running and guest server is accepting commands.
    Running,

    /// Box is shutting down gracefully.
    Stopping,

    /// Box is not running. VM process terminated.
    /// Rootfs is preserved, box can be restarted.
    Stopped,
}

impl BoxStatus {
    /// Check if this status represents an active VM (process may be running).
    pub fn is_active(&self) -> bool {
        matches!(self, BoxStatus::Starting | BoxStatus::Running)
    }

    pub fn is_running(&self) -> bool {
        matches!(self, BoxStatus::Running)
    }

    pub fn is_starting(&self) -> bool {
        matches!(self, BoxStatus::Starting)
    }

    pub fn is_stopped(&self) -> bool {
        matches!(self, BoxStatus::Stopped)
    }

    /// Check if this status represents a transient state.
    pub fn is_transient(&self) -> bool {
        matches!(self, BoxStatus::Starting | BoxStatus::Stopping)
    }

    /// Check if the box can be restarted from this state.
    pub fn can_restart(&self) -> bool {
        matches!(self, BoxStatus::Stopped)
    }

    /// Check if stop() can be called from this state.
    ///
    /// Starting boxes can be stopped because the VM was never fully spawned.
    pub fn can_stop(&self) -> bool {
        matches!(self, BoxStatus::Running | BoxStatus::Starting)
    }

    /// Check if remove() can be called from this state.
    ///
    /// Starting boxes can be removed because the VM was never spawned.
    pub fn can_remove(&self) -> bool {
        matches!(
            self,
            BoxStatus::Starting | BoxStatus::Stopped | BoxStatus::Unknown
        )
    }

    /// Check if exec() can be called from this state.
    pub fn can_exec(&self) -> bool {
        matches!(
            self,
            BoxStatus::Starting | BoxStatus::Running | BoxStatus::Stopped
        )
    }

    /// Check if transition to target state is valid.
    pub fn can_transition_to(&self, target: BoxStatus) -> bool {
        use BoxStatus::*;
        matches!(
            (self, target),
            // Unknown can transition to any state (recovery)
            (Unknown, _) |
            // Starting → Running (init success) or Stopped (init failed/crash)
            (Starting, Running) |
            (Starting, Stopped) |
            (Starting, Unknown) |
            // Running → Stopping (graceful) or Stopped (crash)
            (Running, Stopping) |
            (Running, Stopped) |
            (Running, Unknown) |
            // Stopping → Stopped (complete) or Unknown (error)
            (Stopping, Stopped) |
            (Stopping, Unknown) |
            // Stopped → Starting (restart)
            (Stopped, Starting) |
            (Stopped, Unknown)
        )
    }

    /// Convert to string for database storage.
    pub fn as_str(&self) -> &'static str {
        match self {
            BoxStatus::Unknown => "unknown",
            BoxStatus::Starting => "starting",
            BoxStatus::Running => "running",
            BoxStatus::Stopping => "stopping",
            BoxStatus::Stopped => "stopped",
        }
    }

    /// Parse from database string.
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "unknown" => Some(BoxStatus::Unknown),
            "starting" => Some(BoxStatus::Starting),
            "running" => Some(BoxStatus::Running),
            "stopping" => Some(BoxStatus::Stopping),
            "stopped" => Some(BoxStatus::Stopped),
            _ => None,
        }
    }
}

impl std::fmt::Display for BoxStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

/// Dynamic box state (changes during lifecycle).
///
/// This is updated frequently and persisted to database.
/// State transitions are validated before applying.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BoxState {
    /// Current lifecycle status.
    pub status: BoxStatus,
    pub pid: Option<u32>,
    pub container_id: Option<ContainerId>,
    /// Last state change timestamp (UTC).
    pub last_updated: DateTime<Utc>,
}

impl BoxState {
    /// Create initial state for a new box.
    pub fn new() -> Self {
        Self {
            status: BoxStatus::Starting,
            pid: None,
            container_id: None,
            last_updated: Utc::now(),
        }
    }

    /// Attempt state transition with validation.
    ///
    /// Returns error if the transition is not valid.
    pub fn transition_to(&mut self, new_status: BoxStatus) -> BoxliteResult<()> {
        if !self.status.can_transition_to(new_status) {
            return Err(BoxliteError::InvalidState(format!(
                "Cannot transition from {} to {}",
                self.status, new_status
            )));
        }

        self.status = new_status;
        self.last_updated = Utc::now();
        Ok(())
    }

    /// Force set status without validation (for recovery/internal use).
    pub fn force_status(&mut self, status: BoxStatus) {
        self.status = status;
        self.last_updated = Utc::now();
    }

    /// Set status directly (alias for force_status, used by manager).
    pub fn set_status(&mut self, status: BoxStatus) {
        self.force_status(status);
    }

    /// Set PID and update timestamp.
    pub fn set_pid(&mut self, pid: Option<u32>) {
        self.pid = pid;
        self.last_updated = Utc::now();
    }

    /// Mark box as crashed (sets status to Stopped since VM is no longer running).
    ///
    /// In our simplified state model, crashed VMs become Stopped
    /// since the rootfs is preserved and can be restarted.
    /// PID is cleared since the process is no longer alive.
    pub fn mark_crashed(&mut self) {
        self.status = BoxStatus::Stopped;
        self.pid = None;
        self.last_updated = Utc::now();
    }

    /// Reset state after system reboot.
    ///
    /// Active boxes become Stopped since VM rootfs is preserved.
    /// PID is cleared since all processes are gone after reboot.
    pub fn reset_for_reboot(&mut self) {
        if self.status.is_active() {
            self.status = BoxStatus::Stopped;
        }
        self.pid = None;
        self.last_updated = Utc::now();
    }
}

impl Default for BoxState {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_status_is_active() {
        assert!(BoxStatus::Starting.is_active());
        assert!(BoxStatus::Running.is_active());
        assert!(!BoxStatus::Stopping.is_active());
        assert!(!BoxStatus::Stopped.is_active());
        assert!(!BoxStatus::Unknown.is_active());
    }

    #[test]
    fn test_status_can_restart() {
        assert!(!BoxStatus::Starting.can_restart());
        assert!(!BoxStatus::Running.can_restart());
        assert!(!BoxStatus::Stopping.can_restart());
        assert!(BoxStatus::Stopped.can_restart());
        assert!(!BoxStatus::Unknown.can_restart());
    }

    #[test]
    fn test_status_can_stop() {
        assert!(BoxStatus::Starting.can_stop()); // Starting boxes can be stopped
        assert!(BoxStatus::Running.can_stop());
        assert!(!BoxStatus::Stopping.can_stop());
        assert!(!BoxStatus::Stopped.can_stop());
        assert!(!BoxStatus::Unknown.can_stop());
    }

    #[test]
    fn test_status_can_exec() {
        assert!(BoxStatus::Starting.can_exec());
        assert!(BoxStatus::Running.can_exec());
        assert!(!BoxStatus::Stopping.can_exec());
        assert!(BoxStatus::Stopped.can_exec()); // Triggers restart
        assert!(!BoxStatus::Unknown.can_exec());
    }

    #[test]
    fn test_valid_transitions() {
        // Starting transitions
        assert!(BoxStatus::Starting.can_transition_to(BoxStatus::Running));
        assert!(BoxStatus::Starting.can_transition_to(BoxStatus::Stopped));
        assert!(!BoxStatus::Starting.can_transition_to(BoxStatus::Stopping));

        // Running transitions
        assert!(BoxStatus::Running.can_transition_to(BoxStatus::Stopping));
        assert!(BoxStatus::Running.can_transition_to(BoxStatus::Stopped));
        assert!(!BoxStatus::Running.can_transition_to(BoxStatus::Starting));

        // Stopping transitions
        assert!(BoxStatus::Stopping.can_transition_to(BoxStatus::Stopped));
        assert!(!BoxStatus::Stopping.can_transition_to(BoxStatus::Running));
        assert!(!BoxStatus::Stopping.can_transition_to(BoxStatus::Starting));

        // Stopped transitions
        assert!(BoxStatus::Stopped.can_transition_to(BoxStatus::Starting));
        assert!(!BoxStatus::Stopped.can_transition_to(BoxStatus::Running));
        assert!(!BoxStatus::Stopped.can_transition_to(BoxStatus::Stopping));

        // Unknown can go anywhere
        assert!(BoxStatus::Unknown.can_transition_to(BoxStatus::Starting));
        assert!(BoxStatus::Unknown.can_transition_to(BoxStatus::Running));
        assert!(BoxStatus::Unknown.can_transition_to(BoxStatus::Stopped));
    }

    #[test]
    fn test_state_transition() {
        let mut state = BoxState::new();
        assert_eq!(state.status, BoxStatus::Starting);

        // Valid: Starting → Running
        assert!(state.transition_to(BoxStatus::Running).is_ok());
        assert_eq!(state.status, BoxStatus::Running);

        // Valid: Running → Stopping
        assert!(state.transition_to(BoxStatus::Stopping).is_ok());
        assert_eq!(state.status, BoxStatus::Stopping);

        // Valid: Stopping → Stopped
        assert!(state.transition_to(BoxStatus::Stopped).is_ok());
        assert_eq!(state.status, BoxStatus::Stopped);

        // Valid: Stopped → Starting (restart)
        assert!(state.transition_to(BoxStatus::Starting).is_ok());
        assert_eq!(state.status, BoxStatus::Starting);
    }

    #[test]
    fn test_invalid_transition() {
        let mut state = BoxState::new();
        state.status = BoxStatus::Stopped;

        // Invalid: Stopped → Running (must go through Starting)
        let result = state.transition_to(BoxStatus::Running);
        assert!(result.is_err());
        assert_eq!(state.status, BoxStatus::Stopped); // Unchanged
    }

    #[test]
    fn test_reset_for_reboot() {
        let mut state = BoxState::new();
        state.status = BoxStatus::Running;
        state.pid = Some(12345);

        state.reset_for_reboot();

        assert_eq!(state.status, BoxStatus::Stopped);
        assert_eq!(state.pid, None);
    }

    #[test]
    fn test_reset_for_reboot_stopped() {
        let mut state = BoxState::new();
        state.status = BoxStatus::Stopped;
        state.pid = None;

        state.reset_for_reboot();

        // Stopped stays stopped
        assert_eq!(state.status, BoxStatus::Stopped);
    }

    #[test]
    fn test_status_as_str() {
        assert_eq!(BoxStatus::Unknown.as_str(), "unknown");
        assert_eq!(BoxStatus::Starting.as_str(), "starting");
        assert_eq!(BoxStatus::Running.as_str(), "running");
        assert_eq!(BoxStatus::Stopping.as_str(), "stopping");
        assert_eq!(BoxStatus::Stopped.as_str(), "stopped");
    }

    #[test]
    fn test_status_from_str() {
        assert_eq!(BoxStatus::from_str("unknown"), Some(BoxStatus::Unknown));
        assert_eq!(BoxStatus::from_str("starting"), Some(BoxStatus::Starting));
        assert_eq!(BoxStatus::from_str("running"), Some(BoxStatus::Running));
        assert_eq!(BoxStatus::from_str("stopping"), Some(BoxStatus::Stopping));
        assert_eq!(BoxStatus::from_str("stopped"), Some(BoxStatus::Stopped));
        assert_eq!(BoxStatus::from_str("invalid"), None);
    }
}
