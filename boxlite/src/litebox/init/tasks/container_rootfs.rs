//! Task: Container rootfs preparation.
//!
//! Pulls container image and prepares container rootfs:
//! - Disk-based: Creates ext4 disk image from merged layers (fast boot)
//! - Overlayfs: Extracts layers for guest-side overlayfs (flexible)
//!
//! For restart (reuse_rootfs=true), opens existing COW disk instead of creating new.

use super::{InitCtx, log_task_error, task_start};
use crate::disk::{BackingFormat, Disk, DiskFormat, Qcow2Helper, create_ext4_from_dir};
use crate::images::ContainerConfig;
use crate::litebox::init::types::{
    ContainerRootfsInput, ContainerRootfsOutput, ContainerRootfsPrepResult, USE_DISK_ROOTFS,
    USE_OVERLAYFS,
};
use crate::pipeline::PipelineTask;
use async_trait::async_trait;
use boxlite_shared::errors::{BoxliteError, BoxliteResult};

pub struct ContainerRootfsTask;

#[async_trait]
impl PipelineTask<InitCtx> for ContainerRootfsTask {
    async fn run(self: Box<Self>, ctx: InitCtx) -> BoxliteResult<()> {
        let task_name = self.name();
        let box_id = task_start(&ctx, task_name).await;

        let (options, runtime, layout, reuse_rootfs) = {
            let ctx = ctx.lock().await;
            let layout = ctx
                .fs_output
                .as_ref()
                .ok_or_else(|| BoxliteError::Internal("filesystem task must run first".into()))?
                .layout
                .clone();
            (
                ctx.config.options.clone(),
                ctx.runtime.clone(),
                layout,
                ctx.should_reuse_rootfs(),
            )
        };

        let output = run_container_rootfs(ContainerRootfsInput {
            options: &options,
            runtime: &runtime,
            layout: &layout,
            reuse_rootfs,
        })
        .await
        .inspect_err(|e| log_task_error(&box_id, task_name, e))?;

        let mut ctx = ctx.lock().await;
        ctx.rootfs_output = Some(output);

        Ok(())
    }

    fn name(&self) -> &str {
        "container_rootfs_prep"
    }
}

/// Pull image and prepare rootfs, then create or reuse COW disk.
///
/// For fresh start: creates new COW disk from base image
/// For restart: reuses existing COW disk (preserves user data)
async fn run_container_rootfs(
    input: ContainerRootfsInput<'_>,
) -> BoxliteResult<ContainerRootfsOutput> {
    let disk_path = input.layout.disk_path();

    // For restart, reuse existing COW disk
    if input.reuse_rootfs {
        tracing::info!(
            disk_path = %disk_path.display(),
            "Restart mode: reusing existing container rootfs disk"
        );

        if !disk_path.exists() {
            return Err(BoxliteError::Storage(format!(
                "Cannot restart: container rootfs disk not found at {}",
                disk_path.display()
            )));
        }

        // Open existing disk as persistent (won't be deleted on drop)
        let disk = Disk::new(disk_path.clone(), DiskFormat::Qcow2, true);

        // For restart, we still need container_config for environment etc.
        // Pull image to get config (uses cache, fast)
        let image_ref = match &input.options.rootfs {
            crate::runtime::options::RootfsSpec::Image(r) => r,
            crate::runtime::options::RootfsSpec::RootfsPath(_) => {
                return Err(BoxliteError::Storage(
                    "Direct rootfs paths not yet supported".into(),
                ));
            }
        };
        let image = pull_image(input.runtime, image_ref).await?;
        let image_config = image.load_config().await?;
        let mut container_config = ContainerConfig::from_oci_config(&image_config)?;
        if !input.options.env.is_empty() {
            container_config.merge_env(input.options.env.clone());
        }

        // For restart, rootfs_init is not used (container already exists)
        // but we need a placeholder - use DiskImage with empty device path
        return Ok(ContainerRootfsOutput {
            container_config,
            disk,
        });
    }

    // Fresh start: pull image and prepare rootfs
    let image_ref = match &input.options.rootfs {
        crate::runtime::options::RootfsSpec::Image(r) => r,
        crate::runtime::options::RootfsSpec::RootfsPath(_) => {
            return Err(BoxliteError::Storage(
                "Direct rootfs paths not yet supported".into(),
            ));
        }
    };

    // Pull image (returns cached if already pulled)
    let image = pull_image(input.runtime, image_ref).await?;

    // Prepare base rootfs (get or create cached base disk)
    let rootfs_result = if USE_DISK_ROOTFS {
        prepare_disk_rootfs(input.runtime, &image).await?
    } else if USE_OVERLAYFS {
        prepare_overlayfs_layers(&image).await?
    } else {
        return Err(BoxliteError::Storage(
            "Merged rootfs not supported. Use overlayfs or disk rootfs.".into(),
        ));
    };

    // Create COW disk from base
    let disk = create_cow_disk(&rootfs_result, input.layout)?;

    // Load container config
    let image_config = image.load_config().await?;
    let mut container_config = ContainerConfig::from_oci_config(&image_config)?;

    // Merge user environment variables
    if !input.options.env.is_empty() {
        container_config.merge_env(input.options.env.clone());
    }

    Ok(ContainerRootfsOutput {
        container_config,
        disk,
    })
}

/// Create COW disk from base rootfs.
fn create_cow_disk(
    rootfs_result: &ContainerRootfsPrepResult,
    layout: &crate::runtime::layout::BoxFilesystemLayout,
) -> BoxliteResult<Disk> {
    match rootfs_result {
        ContainerRootfsPrepResult::DiskImage {
            base_disk_path,
            disk_size,
        } => {
            let qcow2_helper = Qcow2Helper::new();
            let cow_disk_path = layout.disk_path();
            let temp_disk = qcow2_helper.create_cow_child_disk(
                base_disk_path,
                BackingFormat::Raw,
                &cow_disk_path,
                *disk_size,
            )?;

            // Make disk persistent so it survives stop/restart
            // create_cow_child_disk returns non-persistent disk, but we want to preserve
            // COW disks across box restarts (only delete on remove)
            let disk_path = temp_disk.leak(); // Prevent cleanup
            let disk = Disk::new(disk_path, DiskFormat::Qcow2, true); // persistent=true

            tracing::info!(
                cow_disk = %cow_disk_path.display(),
                base_disk = %base_disk_path.display(),
                "Created container rootfs COW overlay (persistent)"
            );

            Ok(disk)
        }
        ContainerRootfsPrepResult::Layers { .. } => {
            Err(BoxliteError::Internal(
                "Layers mode requires overlayfs - disk creation not applicable".into(),
            ))
        }
        ContainerRootfsPrepResult::Merged(_) => {
            Err(BoxliteError::Internal(
                "Merged mode not supported".into(),
            ))
        }
    }
}

async fn pull_image(
    runtime: &crate::runtime::RuntimeInner,
    image_ref: &str,
) -> BoxliteResult<crate::images::ImageObject> {
    // ImageManager has internal locking - direct access
    runtime.image_manager.pull(image_ref).await
}

async fn prepare_overlayfs_layers(
    image: &crate::images::ImageObject,
) -> BoxliteResult<ContainerRootfsPrepResult> {
    let layer_paths = image.layer_extracted().await?;

    if layer_paths.is_empty() {
        return Err(BoxliteError::Storage(
            "No layers found for overlayfs".into(),
        ));
    }

    let layers_dir = layer_paths[0]
        .parent()
        .ok_or_else(|| BoxliteError::Storage("Layer path has no parent directory".into()))?
        .to_path_buf();

    let layer_names: Vec<String> = layer_paths
        .iter()
        .map(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("unknown")
                .to_string()
        })
        .collect();

    tracing::info!(
        "Prepared {} layers for guest-side overlayfs",
        layer_names.len()
    );

    Ok(ContainerRootfsPrepResult::Layers {
        layers_dir,
        layer_names,
    })
}

/// Prepare disk-based rootfs from image layers.
///
/// This function:
/// 1. Checks if a cached base disk image exists for this image
/// 2. If not, merges layers and creates an ext4 disk image
/// 3. Returns the path to the base disk for COW overlay creation
async fn prepare_disk_rootfs(
    runtime: &crate::runtime::RuntimeInner,
    image: &crate::images::ImageObject,
) -> BoxliteResult<ContainerRootfsPrepResult> {
    // Check if we already have a cached disk image for this image
    if let Some(disk) = image.disk_image().await {
        let disk_path = disk.path().to_path_buf();
        let disk_size = std::fs::metadata(&disk_path)
            .map(|m| m.len())
            .unwrap_or(64 * 1024 * 1024);

        tracing::info!(
            "Using cached disk image: {} ({}MB)",
            disk_path.display(),
            disk_size / (1024 * 1024)
        );

        // Leak the disk to prevent cleanup (it's a cached persistent disk)
        let _ = disk.leak();

        return Ok(ContainerRootfsPrepResult::DiskImage {
            base_disk_path: disk_path,
            disk_size,
        });
    }

    // No cached disk - we need to create one from layers
    tracing::info!("Creating disk image from layers (first run for this image)");

    // Step 1: Extract and merge layers using RootfsBuilder
    let layer_paths = image.layer_extracted().await?;

    if layer_paths.is_empty() {
        return Err(BoxliteError::Storage(
            "No layers found for disk rootfs".into(),
        ));
    }

    // Create a temporary directory for merged rootfs within boxlite home (same filesystem as destination)
    let temp_base = runtime.layout.temp_dir();
    let temp_dir = tempfile::tempdir_in(&temp_base)
        .map_err(|e| BoxliteError::Storage(format!("Failed to create temp directory: {}", e)))?;
    let merged_path = temp_dir.path().join("merged");

    // Use RootfsBuilder to merge layers
    let builder = crate::rootfs::RootfsBuilder::new();
    let _prepared = builder.prepare(merged_path.clone(), image).await?;

    tracing::info!(
        "Merged {} layers into temporary directory",
        layer_paths.len()
    );

    // Step 2: Create ext4 disk image from merged rootfs
    let temp_disk_path = temp_dir.path().join("rootfs.ext4");

    // Use blocking spawn for sync disk creation
    let merged_clone = merged_path.clone();
    let disk_path_clone = temp_disk_path.clone();
    let temp_disk =
        tokio::task::spawn_blocking(move || create_ext4_from_dir(&merged_clone, &disk_path_clone))
            .await
            .map_err(|e| BoxliteError::Internal(format!("Disk creation task failed: {}", e)))??;

    let disk_size = std::fs::metadata(temp_disk.path())
        .map(|m| m.len())
        .unwrap_or(64 * 1024 * 1024);

    tracing::info!(
        "Created ext4 disk image: {} ({}MB)",
        temp_disk.path().display(),
        disk_size / (1024 * 1024)
    );

    // Step 3: Install disk image to cache
    let installed_disk = image.install_disk_image(temp_disk).await?;
    let final_path = installed_disk.path().to_path_buf();

    // Leak the disk to prevent cleanup
    let _ = installed_disk.leak();

    tracing::info!("Installed disk image to cache: {}", final_path.display());

    // Cleanup: temp_dir is dropped automatically

    Ok(ContainerRootfsPrepResult::DiskImage {
        base_disk_path: final_path,
        disk_size,
    })
}
