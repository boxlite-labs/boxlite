//! Initialization tasks.
//!
//! ## Dependency Graph
//!
//! ```text
//! Filesystem ─────┐
//!                 │
//! ContainerRootfs ┼──→ VmmSpawn ──→ GuestConnect ──→ GuestInit
//!                 │
//! GuestRootfs ────┘
//!
//! Starting (new box):
//! - Stage 1 (sequential): [Filesystem]
//! - Stage 2 (parallel):   [ContainerRootfs, GuestRootfs]
//! - Stage 3 (sequential): [VmmSpawn, GuestConnect, GuestInit]
//!
//! Stopped (restart):
//! - Stage 1 (sequential): [Filesystem]
//! - Stage 2 (parallel):   [ContainerRootfs, GuestRootfs]
//! - Stage 3 (sequential): [VmmSpawn, GuestConnect]
//!
//! Running (reattach):
//! - Stage 1 (sequential): [VmmAttach, GuestConnect]
//! ```

mod container_rootfs;
mod filesystem;
mod guest_connect;
mod guest_init;
mod guest_rootfs;
mod vmm_attach;
mod vmm_spawn;

use super::types::InitPipelineContext;
use boxlite_shared::errors::BoxliteError;
use std::sync::Arc;
use tokio::sync::Mutex;

pub type InitCtx = Arc<Mutex<InitPipelineContext>>;

async fn task_start(ctx: &InitCtx, task_name: &str) -> String {
    let box_id = { ctx.lock().await.config.id.clone() };
    tracing::debug!(box_id = %box_id, task = %task_name, "Executing task");
    box_id
}

fn log_task_error(box_id: &str, task_name: &str, err: &BoxliteError) {
    tracing::error!(box_id = %box_id, task = %task_name, "Task failed: {}", err);
}

pub use container_rootfs::ContainerRootfsTask;
pub use filesystem::FilesystemTask;
pub use guest_connect::GuestConnectTask;
pub use guest_init::GuestInitTask;
pub use guest_rootfs::GuestRootfsTask;
pub use vmm_attach::VmmAttachTask;
pub use vmm_spawn::VmmSpawnTask;
