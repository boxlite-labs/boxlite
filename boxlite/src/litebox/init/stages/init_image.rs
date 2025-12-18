//! Stage 3: Init image preparation.
//!
//! Lazily initializes the bootstrap init rootfs as a disk image (shared across all boxes).

use crate::litebox::init::types::{InitImageInput, InitImageOutput};
use crate::rootfs::RootfsBuilder;
use crate::runtime::constants::images;
use crate::runtime::initrf::{InitRootfs, Strategy};
use crate::util;
use crate::volumes::create_ext4_from_dir;
use boxlite_shared::errors::{BoxliteError, BoxliteResult};

/// Get or initialize bootstrap init image.
///
/// **Single Responsibility**: Init rootfs lazy initialization.
pub async fn run(input: InitImageInput<'_>) -> BoxliteResult<InitImageOutput> {
    let init_rootfs = input
        .init_rootfs_cell
        .get_or_try_init(|| async {
            tracing::info!(
                "Initializing bootstrap init image {} (first time only)",
                images::INIT_ROOTFS
            );

            let base_image = pull_init_image(input.runtime).await?;
            let env = extract_env_from_image(&base_image).await?;
            let init_rootfs = prepare_init_rootfs(input.runtime, &base_image, env).await?;

            tracing::info!("Bootstrap init image ready: {:?}", init_rootfs.strategy);

            Ok::<_, BoxliteError>(init_rootfs)
        })
        .await?;

    Ok(InitImageOutput {
        init_rootfs: init_rootfs.clone(),
    })
}

/// Prepare init rootfs as a disk image.
async fn prepare_init_rootfs(
    runtime: &crate::runtime::RuntimeInner,
    base_image: &crate::images::ImageObject,
    env: Vec<(String, String)>,
) -> BoxliteResult<InitRootfs> {
    // Check if we already have a cached disk image
    if let Some(disk) = base_image.disk_image().await {
        // Verify guest binary is not newer than cached disk
        if is_cache_valid(disk.path())? {
            let disk_path = disk.path().to_path_buf();
            tracing::info!("Using cached init disk image: {}", disk_path.display());

            // Leak the disk to prevent cleanup (it's a cached persistent disk)
            let _ = disk.leak();

            return InitRootfs::new(
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
            "Guest binary updated, invalidating cached init disk: {}",
            disk.path().display()
        );
        std::fs::remove_file(disk.path()).ok();
    }

    // No cached disk - create from layers
    tracing::info!("Creating init disk image from layers (first run)");

    // Extract layers to temp directory within boxlite home (same filesystem as destination)
    let temp_base = runtime.non_sync_state.layout.temp_dir();
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
    let temp_disk_path = temp_dir.path().join("init-rootfs.ext4");
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
        "Created init disk: {} ({}MB)",
        temp_disk.path().display(),
        disk_size / (1024 * 1024)
    );

    // Install disk image to cache
    let installed_disk = base_image.install_disk_image(temp_disk).await?;
    let final_path = installed_disk.path().to_path_buf();

    // Leak the disk to prevent cleanup
    let _ = installed_disk.leak();

    tracing::info!("Installed init disk to cache: {}", final_path.display());

    // temp_dir is dropped here, cleaning up the merged directory

    InitRootfs::new(
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

async fn pull_init_image(
    runtime: &crate::runtime::RuntimeInner,
) -> BoxliteResult<crate::images::ImageObject> {
    let image_manager = {
        let state = runtime.acquire_read()?;
        state.image_manager.clone()
    };
    image_manager.pull(images::INIT_ROOTFS).await
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

/// Check if cached init disk is still valid.
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
