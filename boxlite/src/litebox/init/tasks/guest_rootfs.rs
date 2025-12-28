//! Task: Guest rootfs preparation.
//!
//! Lazily initializes the bootstrap guest rootfs as a disk image (shared across all boxes).
//! Then creates or reuses per-box COW overlay disk.

use super::{InitCtx, log_task_error, task_start};
use crate::disk::{BackingFormat, Disk, DiskFormat, Qcow2Helper, create_ext4_from_dir};
use crate::pipeline::PipelineTask;
use crate::rootfs::RootfsBuilder;
use crate::runtime::constants::images;
use crate::runtime::guest_rootfs::{GuestRootfs, Strategy};
use crate::runtime::layout::BoxFilesystemLayout;
use crate::runtime::rt_impl::SharedRuntimeImpl;
use crate::util;
use async_trait::async_trait;
use boxlite_shared::errors::{BoxliteError, BoxliteResult};

pub struct GuestRootfsTask;

#[async_trait]
impl PipelineTask<InitCtx> for GuestRootfsTask {
    async fn run(self: Box<Self>, ctx: InitCtx) -> BoxliteResult<()> {
        let task_name = self.name();
        let box_id = task_start(&ctx, task_name).await;

        let (runtime, layout, reuse_rootfs) = {
            let ctx = ctx.lock().await;
            let layout = ctx
                .layout
                .clone()
                .ok_or_else(|| BoxliteError::Internal("filesystem task must run first".into()))?;
            (ctx.runtime.clone(), layout, ctx.reuse_rootfs)
        };

        let disk = run_guest_rootfs(&runtime, &layout, reuse_rootfs)
            .await
            .inspect_err(|e| log_task_error(&box_id, task_name, e))?;

        let mut ctx = ctx.lock().await;
        ctx.guest_disk = disk;

        Ok(())
    }

    fn name(&self) -> &str {
        "guest_rootfs_init"
    }
}

/// Get or initialize bootstrap guest rootfs, then create/reuse per-box COW disk.
async fn run_guest_rootfs(
    runtime: &SharedRuntimeImpl,
    layout: &BoxFilesystemLayout,
    reuse_rootfs: bool,
) -> BoxliteResult<Option<Disk>> {
    // First, get or create the shared base guest rootfs
    let guest_rootfs = runtime
        .guest_rootfs
        .get_or_try_init(|| async {
            tracing::info!(
                "Initializing bootstrap guest rootfs {} (first time only)",
                images::INIT_ROOTFS
            );

            let base_image = pull_guest_rootfs_image(runtime).await?;
            let env = extract_env_from_image(&base_image).await?;
            let guest_rootfs = prepare_guest_rootfs(runtime, &base_image, env).await?;

            tracing::info!("Bootstrap guest rootfs ready: {:?}", guest_rootfs.strategy);

            Ok::<_, BoxliteError>(guest_rootfs)
        })
        .await?
        .clone();

    // Now create or reuse the per-box COW disk
    let (_updated_guest_rootfs, disk) =
        create_or_reuse_cow_disk(&guest_rootfs, layout, reuse_rootfs)?;

    Ok(disk)
}

/// Create new COW disk or reuse existing one for restart.
fn create_or_reuse_cow_disk(
    guest_rootfs: &GuestRootfs,
    layout: &BoxFilesystemLayout,
    reuse_rootfs: bool,
) -> BoxliteResult<(GuestRootfs, Option<Disk>)> {
    let guest_rootfs_disk_path = layout.root().join("guest-rootfs.qcow2");

    if reuse_rootfs {
        // Restart: reuse existing COW disk
        tracing::info!(
            disk_path = %guest_rootfs_disk_path.display(),
            "Restart mode: reusing existing guest rootfs disk"
        );

        if !guest_rootfs_disk_path.exists() {
            return Err(BoxliteError::Storage(format!(
                "Cannot restart: guest rootfs disk not found at {}",
                guest_rootfs_disk_path.display()
            )));
        }

        // Open existing disk as persistent
        let disk = Disk::new(guest_rootfs_disk_path.clone(), DiskFormat::Qcow2, true);

        // Update guest_rootfs with the COW disk path
        let mut updated = guest_rootfs.clone();
        if let Strategy::Disk { ref disk_path, .. } = guest_rootfs.strategy {
            updated.strategy = Strategy::Disk {
                disk_path: disk_path.clone(), // Keep base path reference
                device_path: None,            // Will be set by VmmSpawnTask
            };
        }

        return Ok((updated, Some(disk)));
    }

    // Fresh start: create new COW disk
    if let Strategy::Disk { ref disk_path, .. } = guest_rootfs.strategy {
        let base_disk_path = disk_path;

        // Get base disk size
        let base_size = std::fs::metadata(base_disk_path)
            .map(|m| m.len())
            .unwrap_or(512 * 1024 * 1024);

        // Create COW child disk
        let qcow2_helper = Qcow2Helper::new();
        let temp_disk = qcow2_helper.create_cow_child_disk(
            base_disk_path,
            BackingFormat::Raw,
            &guest_rootfs_disk_path,
            base_size,
        )?;

        // Make disk persistent so it survives stop/restart
        let disk_path_owned = temp_disk.leak();
        let disk = Disk::new(disk_path_owned, DiskFormat::Qcow2, true);

        tracing::info!(
            cow_disk = %guest_rootfs_disk_path.display(),
            base_disk = %base_disk_path.display(),
            "Created guest rootfs COW overlay (persistent)"
        );

        // Update guest_rootfs with COW disk path
        let mut updated = guest_rootfs.clone();
        updated.strategy = Strategy::Disk {
            disk_path: guest_rootfs_disk_path,
            device_path: None, // Will be set by VmmSpawnTask
        };

        Ok((updated, Some(disk)))
    } else {
        // Non-disk strategy - no COW disk needed
        Ok((guest_rootfs.clone(), None))
    }
}

/// Prepare guest rootfs as a disk image.
async fn prepare_guest_rootfs(
    runtime: &crate::runtime::SharedRuntimeImpl,
    base_image: &crate::images::ImageObject,
    env: Vec<(String, String)>,
) -> BoxliteResult<GuestRootfs> {
    // Check if we already have a cached disk image
    if let Some(disk) = base_image.disk_image().await {
        // Verify guest binary is not newer than cached disk
        if is_cache_valid(disk.path())? {
            let disk_path = disk.path().to_path_buf();
            tracing::info!(
                "Using cached guest rootfs disk image: {}",
                disk_path.display()
            );

            // Leak the disk to prevent cleanup (it's a cached persistent disk)
            let _ = disk.leak();

            return GuestRootfs::new(
                disk_path.clone(),
                Strategy::Disk {
                    disk_path,
                    device_path: None, // Set later in build_disk_attachments
                },
                None,
                None,
                env,
            );
        }

        // Cache invalid - delete and recreate
        tracing::info!(
            "Guest binary updated, invalidating cached guest rootfs disk: {}",
            disk.path().display()
        );
        std::fs::remove_file(disk.path()).ok();
    }

    // No cached disk - create from layers
    tracing::info!("Creating guest rootfs disk image from layers (first run)");

    // Extract layers to temp directory within boxlite home (same filesystem as destination)
    let temp_base = runtime.layout.temp_dir();
    let temp_dir = tempfile::tempdir_in(&temp_base)
        .map_err(|e| BoxliteError::Storage(format!("Failed to create temp directory: {}", e)))?;
    let merged_path = temp_dir.path().join("merged");

    let builder = RootfsBuilder::new();
    let prepared = builder.prepare(merged_path.clone(), base_image).await?;

    // Inject guest binary
    util::inject_guest_binary(&prepared.path)?;

    // Verify guest binary
    let guest_bin_path = prepared.path.join("boxlite/bin/boxlite-guest");
    if guest_bin_path.exists() {
        tracing::info!(
            "Guest binary at: {} ({} bytes)",
            guest_bin_path.display(),
            std::fs::metadata(&guest_bin_path)
                .map(|m| m.len())
                .unwrap_or(0)
        );
    } else {
        return Err(BoxliteError::Storage(format!(
            "Guest binary not found at: {}",
            guest_bin_path.display()
        )));
    }

    // Create ext4 disk from merged directory
    let temp_disk_path = temp_dir.path().join("guest-rootfs.ext4");
    let merged_clone = prepared.path.clone();
    let disk_clone = temp_disk_path.clone();
    let temp_disk =
        tokio::task::spawn_blocking(move || create_ext4_from_dir(&merged_clone, &disk_clone))
            .await
            .map_err(|e| BoxliteError::Internal(format!("Disk creation task failed: {}", e)))??;

    let disk_size = std::fs::metadata(temp_disk.path())
        .map(|m| m.len())
        .unwrap_or(0);

    tracing::info!(
        "Created guest rootfs disk: {} ({}MB)",
        temp_disk.path().display(),
        disk_size / (1024 * 1024)
    );

    // Install disk image to cache
    let installed_disk = base_image.install_disk_image(temp_disk).await?;
    let final_path = installed_disk.path().to_path_buf();

    // Leak the disk to prevent cleanup
    let _ = installed_disk.leak();

    tracing::info!(
        "Installed guest rootfs disk to cache: {}",
        final_path.display()
    );

    // temp_dir is dropped here, cleaning up the merged directory

    GuestRootfs::new(
        final_path.clone(),
        Strategy::Disk {
            disk_path: final_path,
            device_path: None, // Set later in build_disk_attachments
        },
        None,
        None,
        env,
    )
}

async fn pull_guest_rootfs_image(
    runtime: &crate::runtime::SharedRuntimeImpl,
) -> BoxliteResult<crate::images::ImageObject> {
    // ImageManager has internal locking - direct access
    runtime.image_manager.pull(images::INIT_ROOTFS).await
}

async fn extract_env_from_image(
    image: &crate::images::ImageObject,
) -> BoxliteResult<Vec<(String, String)>> {
    let image_config = image.load_config().await?;

    let env: Vec<(String, String)> = if let Some(config) = image_config.config() {
        if let Some(envs) = config.env() {
            envs.iter()
                .filter_map(|e| {
                    let parts: Vec<&str> = e.splitn(2, '=').collect();
                    if parts.len() == 2 {
                        Some((parts[0].to_string(), parts[1].to_string()))
                    } else {
                        None
                    }
                })
                .collect()
        } else {
            Vec::new()
        }
    } else {
        Vec::new()
    };

    Ok(env)
}

/// Check if cached guest rootfs disk is still valid.
///
/// Returns false if the current guest binary is newer than the cached disk,
/// indicating the cache should be invalidated and recreated.
fn is_cache_valid(cache_path: &std::path::Path) -> BoxliteResult<bool> {
    let guest_bin = util::find_binary("boxlite-guest")?;

    let guest_mtime = std::fs::metadata(&guest_bin)
        .and_then(|m| m.modified())
        .map_err(|e| {
            BoxliteError::Storage(format!(
                "Failed to get guest binary mtime {}: {}",
                guest_bin.display(),
                e
            ))
        })?;

    let cache_mtime = std::fs::metadata(cache_path)
        .and_then(|m| m.modified())
        .map_err(|e| {
            BoxliteError::Storage(format!(
                "Failed to get cache mtime {}: {}",
                cache_path.display(),
                e
            ))
        })?;

    // Cache is valid if it's newer than the guest binary
    Ok(cache_mtime >= guest_mtime)
}
