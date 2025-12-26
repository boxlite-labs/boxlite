pub mod process;

use std::path::PathBuf;
use std::process::Command;

use boxlite_shared::errors::{BoxliteError, BoxliteResult};
use tracing_appender::non_blocking::NonBlocking;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, fmt};

// Re-export process utilities
pub use process::{is_process_alive, is_same_process, kill_process};

#[cfg(any(target_os = "linux", target_os = "macos"))]
unsafe extern "C" {
    fn dladdr(addr: *const libc::c_void, info: *mut libc::Dl_info) -> libc::c_int;
}

struct LibraryLoadPath;

impl LibraryLoadPath {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn get_library_path_via_dladdr(
        default_addr: *const libc::c_void,
        addr: Option<*const libc::c_void>,
    ) -> Option<PathBuf> {
        use libc::Dl_info;
        use std::ffi::CStr;

        let mut info: Dl_info = unsafe { std::mem::zeroed() };
        let result = unsafe { dladdr(addr.unwrap_or(default_addr), &mut info) };

        if result != 0 && !info.dli_fname.is_null() {
            let c_str = unsafe { CStr::from_ptr(info.dli_fname) };
            let path = c_str.to_string_lossy().into_owned();
            Some(PathBuf::from(path))
        } else {
            None
        }
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn get(addr: Option<*const libc::c_void>) -> Option<PathBuf> {
        Self::get_library_path_via_dladdr(Self::get as *const libc::c_void, addr)
    }

    #[cfg(target_os = "windows")]
    fn get(addr: Option<*const libc::c_void>) -> Option<PathBuf> {
        use std::ffi::OsString;
        use std::os::windows::ffi::OsStringExt;
        use std::ptr;
        use winapi::um::libloaderapi::GetModuleFileNameW;
        use winapi::um::libloaderapi::GetModuleHandleExW;
        use winapi::um::winnt::HANDLE;

        let mut handle: HANDLE = ptr::null_mut();
        let flags = 0x00000004; // GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS
        let ok = unsafe {
            GetModuleHandleExW(
                flags,
                addr.unwrap_or(Self::get as *const libc::c_void),
                &mut handle,
            )
        };
        if ok == 0 {
            return None;
        }

        let mut buffer = [0u16; 260];
        let len = unsafe { GetModuleFileNameW(handle, buffer.as_mut_ptr(), buffer.len() as u32) };
        if len == 0 {
            return None;
        }

        Some(PathBuf::from(OsString::from_wide(&buffer[..len as usize])))
    }
}

/// Configure dynamic library search paths for the Box runner command.
///
/// This ensures engine libraries bundled alongside the runner are
/// discoverable when the subprocess starts.
pub fn configure_library_env(cmd: &mut Command, addr: *const libc::c_void) {
    if let Some(runner_dir) = LibraryLoadPath::get(Some(addr)) {
        let dylibs_path = runner_dir.parent();
        tracing::debug!("dylibs_path: {:?}", dylibs_path);

        if let Some(dylibs) = dylibs_path
            && dylibs.exists()
        {
            #[cfg(target_os = "macos")]
            {
                let fallback_path =
                    if let Ok(existing) = std::env::var("DYLD_FALLBACK_LIBRARY_PATH") {
                        format!("{}:{}", dylibs.display(), existing)
                    } else {
                        dylibs.display().to_string()
                    };
                cmd.env("DYLD_FALLBACK_LIBRARY_PATH", fallback_path);
                tracing::debug!(dylibs = %dylibs.display(), "Set DYLD_FALLBACK_LIBRARY_PATH for bundled libraries");
            }

            #[cfg(target_os = "linux")]
            {
                let lib_path = if let Ok(existing) = std::env::var("LD_LIBRARY_PATH") {
                    format!("{}:{}", dylibs.display(), existing)
                } else {
                    dylibs.display().to_string()
                };
                cmd.env("LD_LIBRARY_PATH", lib_path);
                tracing::debug!(dylibs = %dylibs.display(), "Set LD_LIBRARY_PATH for bundled libraries");
            }
        }
    }
}

/// Find the Box runner binary in common locations.
///
/// # Arguments
/// * `binary_name` - Name of the binary to find (e.g., "boxlite-shim")
///
/// # Returns
/// * `Ok(PathBuf)` - Path to the found binary
/// * `Err(...)` - Binary not found in any expected location
pub fn find_binary(binary_name: &str) -> BoxliteResult<PathBuf> {
    let mut candidates = Vec::new();

    if let Some(runner_dir) = LibraryLoadPath::get(None) {
        let boxlite_runtime_path = runner_dir.parent().map(|p| p.join("runtime"));
        candidates.push(boxlite_runtime_path.unwrap().join(binary_name));
    }

    if let Ok(boxlite_runtime_dir) = std::env::var("BOXLITE_RUNTIME_DIR") {
        candidates.push(PathBuf::from(boxlite_runtime_dir).join(binary_name));
    }

    // Try all candidates
    for candidate in &candidates {
        tracing::debug!("Finding binary {:?} in path: {:?}", binary_name, candidate);
        if candidate.exists() {
            tracing::debug!(binary = %candidate.display(), "Found binary");
            return Ok(candidate.clone());
        }
    }

    // Not found - return error with all searched locations
    let locations = candidates
        .iter()
        .map(|p| format!("  - {}", p.display()))
        .collect::<Vec<_>>()
        .join("\n");

    Err(BoxliteError::Storage(format!(
        "Binary '{}' not found.\nSearched locations:\n{}",
        binary_name, locations
    )))
}

pub fn register_to_tracing(non_blocking: NonBlocking, env_filter: EnvFilter) {
    let _ = tracing_subscriber::registry()
        .with(env_filter)
        .with(
            fmt::layer()
                .with_writer(non_blocking)
                .with_target(true)
                .with_thread_ids(false)
                .with_file(false)
                .with_line_number(false)
                .with_ansi(false),
        )
        .try_init();
}

/// Inject guest binary into a rootfs directory.
///
/// Copies boxlite-guest into `/boxlite/bin/` so it can be executed
/// directly without needing a virtiofs mount at boot time.
///
/// Uses fast mtime+size comparison to avoid unnecessary copies.
/// The binary is only copied if:
/// - It doesn't exist in the destination
/// - The sizes differ
/// - The source is newer than the destination
pub fn inject_guest_binary(rootfs_path: &std::path::Path) -> BoxliteResult<()> {
    let dest_dir = rootfs_path.join("boxlite/bin");
    let dest_path = dest_dir.join("boxlite-guest");
    let guest_bin = find_binary("boxlite-guest")?;

    // Check if binary needs update
    if dest_path.exists() {
        if is_binary_up_to_date(&guest_bin, &dest_path)? {
            return Ok(());
        }
        // Remove old binary before copying (it might be read-only 0o555)
        std::fs::remove_file(&dest_path).map_err(|e| {
            BoxliteError::Storage(format!(
                "Failed to remove old guest binary {}: {}",
                dest_path.display(),
                e
            ))
        })?;
    }

    std::fs::create_dir_all(&dest_dir).map_err(|e| {
        BoxliteError::Storage(format!(
            "Failed to create guest bin directory {}: {}",
            dest_dir.display(),
            e
        ))
    })?;

    std::fs::copy(&guest_bin, &dest_path).map_err(|e| {
        BoxliteError::Storage(format!(
            "Failed to copy guest binary to {}: {}",
            dest_path.display(),
            e
        ))
    })?;

    // Ensure executable
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dest_path, std::fs::Permissions::from_mode(0o555)).map_err(
            |e| {
                BoxliteError::Storage(format!(
                    "Failed to set permissions on {}: {}",
                    dest_path.display(),
                    e
                ))
            },
        )?;
    }

    tracing::info!("Injected guest binary into {}", dest_path.display());
    Ok(())
}

/// Check if destination binary is up-to-date compared to source.
///
/// Uses fast mtime+size comparison instead of content hashing.
fn is_binary_up_to_date(source: &std::path::Path, dest: &std::path::Path) -> BoxliteResult<bool> {
    let source_meta = std::fs::metadata(source).map_err(|e| {
        BoxliteError::Storage(format!(
            "Failed to get metadata for {}: {}",
            source.display(),
            e
        ))
    })?;

    let dest_meta = std::fs::metadata(dest).map_err(|e| {
        BoxliteError::Storage(format!(
            "Failed to get metadata for {}: {}",
            dest.display(),
            e
        ))
    })?;

    // Quick rejection: different sizes means definitely different content
    if source_meta.len() != dest_meta.len() {
        tracing::debug!(
            "Guest binary size changed ({} -> {} bytes)",
            dest_meta.len(),
            source_meta.len()
        );
        return Ok(false);
    }

    // Compare modification times
    let source_mtime = source_meta.modified().map_err(|e| {
        BoxliteError::Storage(format!(
            "Failed to get mtime for {}: {}",
            source.display(),
            e
        ))
    })?;

    let dest_mtime = dest_meta.modified().map_err(|e| {
        BoxliteError::Storage(format!("Failed to get mtime for {}: {}", dest.display(), e))
    })?;

    if dest_mtime >= source_mtime {
        tracing::debug!(
            "Guest binary at {} is up-to-date (size: {} bytes)",
            dest.display(),
            dest_meta.len()
        );
        return Ok(true);
    }

    tracing::debug!("Guest binary source is newer than destination");
    Ok(false)
}

/// Auto-detect terminal size like Docker does
/// Returns (rows, cols) tuple
pub fn get_terminal_size() -> (u32, u32) {
    // Try to get terminal size from environment or use standard defaults
    if let Some((cols, rows)) = term_size::dimensions() {
        (rows as u32, cols as u32)
    } else {
        // Standard terminal size (80x24)
        (24, 80)
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn test_xattr_format_with_leading_zeros() {
        // Test that xattr values are formatted with 4-digit octal (leading zeros)
        let test_cases = vec![
            (0o755, "0:0:0755"),  // rwxr-xr-x
            (0o644, "0:0:0644"),  // rw-r--r--
            (0o700, "0:0:0700"),  // rwx------
            (0o555, "0:0:0555"),  // r-xr-xr-x
            (0o777, "0:0:0777"),  // rwxrwxrwx
            (0o000, "0:0:0000"),  // ---------
            (0o4755, "0:0:4755"), // rwsr-xr-x (setuid)
            (0o2755, "0:0:2755"), // rwxr-sr-x (setgid)
            (0o1755, "0:0:1755"), // rwxr-xr-t (sticky)
        ];

        for (mode, expected) in test_cases {
            let actual = format!("0:0:{:04o}", mode & 0o7777);
            assert_eq!(
                actual, expected,
                "Mode {:o} should format to '{}', got '{}'",
                mode, expected, actual
            );
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn test_fix_rootfs_permissions_basic() {
        use crate::rootfs::operations::fix_rootfs_permissions;
        use std::fs;
        use std::os::unix::fs::PermissionsExt;
        use std::path::Path;
        use tempfile::TempDir;

        // Create a temporary directory structure
        let temp_dir = TempDir::new().unwrap();
        let rootfs = temp_dir.path();

        // Set rootfs to 0700 (like a real rootfs would be)
        let mut perms = fs::metadata(rootfs).unwrap().permissions();
        perms.set_mode(0o700);
        fs::set_permissions(rootfs, perms).unwrap();

        // Create test files with different permissions
        let file1 = rootfs.join("executable");
        fs::write(&file1, "#!/bin/sh\necho test").unwrap();
        let mut perms = fs::metadata(&file1).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&file1, perms).unwrap();

        let file2 = rootfs.join("readonly");
        fs::write(&file2, "data").unwrap();
        let mut perms = fs::metadata(&file2).unwrap().permissions();
        perms.set_mode(0o444);
        fs::set_permissions(&file2, perms).unwrap();

        let dir = rootfs.join("subdir");
        fs::create_dir(&dir).unwrap();
        let mut perms = fs::metadata(&dir).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&dir, perms).unwrap();

        // Create a symlink
        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            symlink(&file1, rootfs.join("link")).unwrap();
        }

        // Run the fix_rootfs_permissions function
        let result = fix_rootfs_permissions(rootfs);
        assert!(
            result.is_ok(),
            "fix_rootfs_permissions failed: {:?}",
            result.err()
        );

        // Verify xattr was set on regular files and directories
        let check_xattr = |path: &Path, expected_mode: u32| {
            let xattr_value = xattr::get(path, "user.containers.override_stat")
                .unwrap_or_else(|_| panic!("Failed to read xattr from {:?}", path))
                .unwrap_or_else(|| panic!("xattr not set on {:?}", path));
            let expected = format!("0:0:{:04o}", expected_mode);
            assert_eq!(
                String::from_utf8_lossy(&xattr_value),
                expected,
                "xattr mismatch for {:?}",
                path
            );
        };

        // Root directory should be 700
        check_xattr(rootfs, 0o700);

        // Executable should preserve 755
        check_xattr(&file1, 0o755);

        // Readonly should preserve 444
        check_xattr(&file2, 0o444);

        // Directory should preserve 755
        check_xattr(&dir, 0o755);

        // Verify symlinks don't get xattr (skipped intentionally)
        let symlink_path = rootfs.join("link");
        let symlink_xattr = xattr::get(&symlink_path, "user.containers.override_stat").unwrap();
        assert!(
            symlink_xattr.is_none(),
            "Symlinks should not have xattr set"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn test_fix_rootfs_permissions_preserves_setuid() {
        use crate::rootfs::operations::fix_rootfs_permissions;
        use std::fs;
        use std::os::unix::fs::PermissionsExt;
        use tempfile::TempDir;

        let temp_dir = TempDir::new().unwrap();
        let rootfs = temp_dir.path();

        // Create a file with setuid bit
        let setuid_file = rootfs.join("setuid_binary");
        fs::write(&setuid_file, "binary").unwrap();
        let mut perms = fs::metadata(&setuid_file).unwrap().permissions();
        perms.set_mode(0o4755); // setuid + rwxr-xr-x
        fs::set_permissions(&setuid_file, perms).unwrap();

        // Run fix_rootfs_permissions
        fix_rootfs_permissions(rootfs).unwrap();

        // Verify setuid bit is preserved in xattr
        let xattr_value = xattr::get(&setuid_file, "user.containers.override_stat")
            .unwrap()
            .expect("xattr not set");
        assert_eq!(
            String::from_utf8_lossy(&xattr_value),
            "0:0:4755",
            "Setuid bit should be preserved in xattr"
        );
    }
}
