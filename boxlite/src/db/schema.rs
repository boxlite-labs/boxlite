//! Database schema definitions.
//!
//! Uses Podman-style pattern:
//! - BoxConfig: Immutable configuration (set at creation)
//! - BoxState: Mutable state (changes during lifecycle)
//!
//! Each table has queryable columns for efficient filtering + JSON blob for full data.

/// Current schema version.
pub const SCHEMA_VERSION: i32 = 2;

/// Schema version tracking table.
pub const SCHEMA_VERSION_TABLE: &str = r#"
CREATE TABLE IF NOT EXISTS schema_version (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    version INTEGER NOT NULL,
    updated_at TEXT NOT NULL
);
"#;

/// BoxConfig table schema.
///
/// Stores immutable box configuration. JSON blob contains full BoxConfig struct.
/// Queryable columns: id, created_at (for sorting/filtering).
pub const BOX_CONFIG_TABLE: &str = r#"
CREATE TABLE IF NOT EXISTS box_config (
    id TEXT PRIMARY KEY NOT NULL,
    created_at INTEGER NOT NULL,
    json TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_box_config_created_at ON box_config(created_at);
"#;

/// BoxState table schema.
///
/// Stores mutable box state. JSON blob contains full BoxState struct.
/// Queryable columns: id, status, pid (for filtering active boxes).
pub const BOX_STATE_TABLE: &str = r#"
CREATE TABLE IF NOT EXISTS box_state (
    id TEXT PRIMARY KEY NOT NULL,
    status TEXT NOT NULL,
    pid INTEGER,
    json TEXT NOT NULL,
    FOREIGN KEY (id) REFERENCES box_config(id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS idx_box_state_status ON box_state(status);
CREATE INDEX IF NOT EXISTS idx_box_state_pid ON box_state(pid);
"#;

/// Alive file table schema.
///
/// Tracks runtime instance. If stale on startup, indicates reboot.
pub const ALIVE_TABLE: &str = r#"
CREATE TABLE IF NOT EXISTS alive (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    boot_id TEXT NOT NULL,
    started_at INTEGER NOT NULL
);
"#;

/// Get all schema creation statements.
pub fn all_schemas() -> Vec<&'static str> {
    vec![
        SCHEMA_VERSION_TABLE,
        BOX_CONFIG_TABLE,
        BOX_STATE_TABLE,
        ALIVE_TABLE,
    ]
}
