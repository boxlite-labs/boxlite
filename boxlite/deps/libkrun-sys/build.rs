use std::collections::HashMap;
use std::env;
#[cfg(target_os = "macos")]
use std::fs;
#[cfg(target_os = "macos")]
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

// libkrunfw prebuilt tarball configuration (contains kernel.c) - macOS only
#[cfg(target_os = "macos")]
const LIBKRUNFW_VERSION: &str = "4.10.0";
#[cfg(target_os = "macos")]
const LIBKRUNFW_PREBUILT_URL: &str = "https://github.com/containers/libkrunfw/releases/download/v4.10.0/libkrunfw-4.10.0-prebuilt-aarch64.tar.gz";
#[cfg(target_os = "macos")]
const LIBKRUNFW_SHA256: &str = "6732e0424ce90fa246a4a75bb5f3357a883546dbca095fee07a7d587e82d94b0";

// Cross-compilation patch for building init binary on macOS (vendored locally)
#[cfg(target_os = "macos")]
const LIBKRUN_PATCH_FILE: &str = "patches/macos-cross-compile.patch";

// libkrun build features (NET=1 BLK=1 enables network and block device support)
const LIBKRUN_BUILD_FEATURES: &[(&str, &str)] = &[("NET", "1"), ("BLK", "1")];

// Library directory name differs by platform
#[cfg(target_os = "macos")]
const LIB_DIR: &str = "lib";
#[cfg(target_os = "linux")]
const LIB_DIR: &str = "lib64";

/// Returns libkrun build environment with features enabled.
fn libkrun_build_env(libkrunfw_install: &Path) -> HashMap<String, String> {
    let mut env = HashMap::new();
    env.insert(
        "PKG_CONFIG_PATH".to_string(),
        format!("{}/{}/pkgconfig", libkrunfw_install.display(), LIB_DIR),
    );
    for (key, value) in LIBKRUN_BUILD_FEATURES {
        env.insert(key.to_string(), value.to_string());
    }
    env
}

/// Verifies vendored sources exist.
fn verify_vendored_sources(manifest_dir: &Path, require_libkrunfw: bool) {
    let libkrun_src = manifest_dir.join("vendor/libkrun");
    let libkrunfw_src = manifest_dir.join("vendor/libkrunfw");

    let missing_libkrun = !libkrun_src.exists();
    let missing_libkrunfw = require_libkrunfw && !libkrunfw_src.exists();

    if missing_libkrun || missing_libkrunfw {
        eprintln!("ERROR: Vendored sources not found");
        eprintln!();
        eprintln!("Initialize git submodules:");
        eprintln!("  git submodule update --init --recursive");
        std::process::exit(1);
    }
}

fn main() {
    // Rebuild if vendored sources change
    println!("cargo:rerun-if-changed=vendor/libkrun");
    println!("cargo:rerun-if-changed=vendor/libkrunfw");

    // Check for stub mode (for CI linting without building)
    // Set BOXLITE_DEPS_STUB=1 to skip building and emit stub link directives
    if env::var("BOXLITE_DEPS_STUB").is_ok() {
        println!("cargo:warning=BOXLITE_DEPS_STUB mode: skipping libkrun build");
        // Emit minimal link directives that won't actually link anything
        // This allows cargo check/clippy to pass without building libkrun
        println!("cargo:rustc-link-lib=dylib=krun");
        // Use a non-existent path - linking will fail but check/clippy won't try to link
        println!("cargo:LIBKRUN_BOXLITE_DEP=/nonexistent");
        println!("cargo:LIBKRUNFW_BOXLITE_DEP=/nonexistent");
        return;
    }

    build();
}

/// Runs a command and panics with a helpful message if it fails.
#[allow(unused)]
fn run_command(cmd: &mut Command, description: &str) {
    let status = cmd
        .status()
        .unwrap_or_else(|e| panic!("Failed to execute {}: {}", description, e));

    if !status.success() {
        panic!("{} failed with exit code: {:?}", description, status.code());
    }
}

/// Checks if a directory contains any library file matching the given prefix.
/// Returns true if a file like "prefix.*.{dylib,so}" exists.
#[allow(unused)]
fn has_library(dir: &Path, prefix: &str) -> bool {
    let extensions = if cfg!(target_os = "macos") {
        vec!["dylib"]
    } else if cfg!(target_os = "linux") {
        vec!["so"]
    } else {
        vec!["dll"]
    };

    dir.read_dir()
        .ok()
        .map(|entries| {
            entries.filter_map(Result::ok).any(|entry| {
                let filename = entry.file_name().to_string_lossy().to_string();
                filename.starts_with(prefix)
                    && extensions
                        .iter()
                        .any(|ext| entry.path().extension().is_some_and(|e| e == *ext))
            })
        })
        .unwrap_or(false)
}

/// Creates a make command with common configuration.
#[allow(unused)]
fn make_command(
    source_dir: &Path,
    install_dir: &Path,
    extra_env: &HashMap<String, String>,
) -> Command {
    let mut cmd = Command::new("make");
    cmd.stdout(Stdio::inherit());
    cmd.stderr(Stdio::inherit());
    cmd.args(["-j", &num_cpus::get().to_string()])
        .arg("MAKEFLAGS=") // Clear MAKEFLAGS to prevent -w flag issues in submakes
        .env("PREFIX", install_dir)
        .current_dir(source_dir);

    // Apply extra environment variables
    for (key, value) in extra_env {
        cmd.env(key, value);
    }

    cmd
}

/// Builds a library using Make with the specified parameters.
#[allow(unused)]
fn build_with_make(
    source_dir: &Path,
    install_dir: &Path,
    lib_name: &str,
    extra_env: HashMap<String, String>,
) {
    println!("cargo:warning=Building {} from source...", lib_name);

    std::fs::create_dir_all(install_dir)
        .unwrap_or_else(|e| panic!("Failed to create install directory: {}", e));

    // Build
    let mut make_cmd = make_command(source_dir, install_dir, &extra_env);
    run_command(&mut make_cmd, &format!("make {}", lib_name));

    // Install
    let mut install_cmd = make_command(source_dir, install_dir, &extra_env);
    install_cmd.arg("install");
    run_command(&mut install_cmd, &format!("make install {}", lib_name));
}

/// Configure linking for libkrun.
///
/// Note: libkrunfw is NOT linked here - it's dlopened by libkrun at runtime.
/// We only expose the library directory so downstream crates can bundle it.
fn configure_linking(libkrun_dir: &Path, libkrunfw_dir: &Path) {
    println!("cargo:rustc-link-search=native={}", libkrun_dir.display());
    println!("cargo:rustc-link-lib=dylib=krun");

    // Expose library directories to downstream crates (used by boxlite/build.rs)
    // Convention: {LIBNAME}_BOXLITE_DEP=<path> for auto-discovery
    println!("cargo:LIBKRUN_BOXLITE_DEP={}", libkrun_dir.display());
    println!("cargo:LIBKRUNFW_BOXLITE_DEP={}", libkrunfw_dir.display());
}

/// Fixes the install_name on macOS to use an absolute path.
/// This allows install_name_tool to modify the library path during wheel repair.
#[cfg(target_os = "macos")]
fn fix_install_name(lib_name: &str, lib_path: &Path) {
    let status = Command::new("install_name_tool")
        .args([
            "-id",
            &format!("@rpath/{}", lib_name),
            lib_path.to_str().unwrap(),
        ])
        .status()
        .expect("Failed to execute install_name_tool");

    if !status.success() {
        panic!("Failed to set install_name for {}", lib_name);
    }
}

#[cfg(target_os = "linux")]
fn fix_install_name(lib_name: &str, lib_path: &Path) {
    let lib_path_str = lib_path.to_str().expect("Invalid library path");

    println!("cargo:warning=Fixing {} in {}", lib_name, lib_path_str);

    let status = Command::new("patchelf")
        .args([
            "--set-soname",
            lib_name, // On Linux, SONAME is just the library name, not @rpath/name
            lib_path_str,
        ])
        .status()
        .expect("Failed to execute patchelf");

    if !status.success() {
        panic!("Failed to set install_name for {}", lib_name);
    }
}

/// Extract SONAME from versioned library filename
/// e.g., libkrunfw.so.4.9.0 -> Some("libkrunfw.so.4")
///       libkrun.so.1.15.1 -> Some("libkrun.so.1")
#[allow(dead_code)]
fn extract_major_soname(filename: &str) -> Option<String> {
    // Find ".so." pattern
    if let Some(so_pos) = filename.find(".so.") {
        let base = &filename[..so_pos + 3]; // "libkrunfw.so"
        let versions = &filename[so_pos + 4..]; // "4.9.0"

        // Get first number (major version)
        if let Some(major) = versions.split('.').next() {
            return Some(format!("{}.{}", base, major)); // "libkrunfw.so.4"
        }
    }
    None
}

#[cfg(target_os = "linux")]
fn fix_linux_libs(src_dir: &Path, lib_prefix: &str) -> Result<(), String> {
    use std::fs;

    // First pass: copy regular files and record symlinks
    for entry in
        fs::read_dir(src_dir).map_err(|e| format!("Failed to read source directory: {}", e))?
    {
        let entry = entry.map_err(|e| format!("Failed to read entry: {}", e))?;
        let path = entry.path();
        let filename = path.file_name().unwrap().to_string_lossy().to_string();

        if filename.starts_with(lib_prefix) && filename.contains(".so") {
            // Check if it's a symlink
            let metadata = fs::symlink_metadata(&path)
                .map_err(|e| format!("Failed to get metadata: {}", e))?;

            if metadata.file_type().is_symlink() {
                continue;
            } else {
                // For libkrunfw only: rename to major version
                if lib_prefix == "libkrunfw" {
                    if let Some(soname) = extract_major_soname(&filename) {
                        if soname != filename {
                            let new_path = src_dir.join(&soname);
                            fs::rename(&path, &new_path)
                                .map_err(|e| format!("Failed to rename file: {}", e))?;
                            println!("cargo:warning=Renamed {} to {}", filename, soname);

                            // Fix install_name on renamed file
                            fix_install_name(&soname, &new_path);
                            continue;
                        }
                    }
                }

                // Fix install_name (only for non-symlinks)
                fix_install_name(&filename, &path);
            }
        }
    }

    Ok(())
}

/// Downloads a file from URL to the specified path.
#[cfg(target_os = "macos")]
fn download_file(url: &str, dest: &Path) -> io::Result<()> {
    println!("cargo:warning=Downloading {}...", url);

    let output = Command::new("curl")
        .args(["-fsSL", "-o", dest.to_str().unwrap(), url])
        .output()?;

    if !output.status.success() {
        return Err(io::Error::other(format!(
            "curl failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }

    Ok(())
}

/// Verifies SHA256 checksum of a file.
#[cfg(target_os = "macos")]
fn verify_sha256(file: &Path, expected: &str) -> io::Result<()> {
    let output = Command::new("shasum")
        .args(["-a", "256", file.to_str().unwrap()])
        .output()?;

    if !output.status.success() {
        return Err(io::Error::other("shasum failed"));
    }

    let actual = String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
        .next()
        .unwrap_or("")
        .to_string();

    if actual != expected {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("SHA256 mismatch: expected {}, got {}", expected, actual),
        ));
    }

    println!("cargo:warning=SHA256 verified: {}", expected);
    Ok(())
}

/// Extracts a tarball to the specified directory.
#[cfg(target_os = "macos")]
fn extract_tarball(tarball: &Path, dest: &Path) -> io::Result<()> {
    fs::create_dir_all(dest)?;

    let status = Command::new("tar")
        .args([
            "-xzf",
            tarball.to_str().unwrap(),
            "-C",
            dest.to_str().unwrap(),
        ])
        .status()?;

    if !status.success() {
        return Err(io::Error::other("tar extraction failed"));
    }

    Ok(())
}

/// Downloads and extracts the prebuilt libkrunfw tarball.
/// Returns the path to the extracted source directory.
#[cfg(target_os = "macos")]
fn download_libkrunfw_prebuilt(out_dir: &Path) -> PathBuf {
    let tarball_path = out_dir.join("libkrunfw-prebuilt.tar.gz");
    let extract_dir = out_dir.join("libkrunfw-src");
    let src_dir = extract_dir.join(format!("libkrunfw-{}", LIBKRUNFW_VERSION));

    // Check if already extracted
    if src_dir.join("kernel.c").exists() {
        println!("cargo:warning=Using cached libkrunfw source");
        return src_dir;
    }

    // Download if not cached
    if !tarball_path.exists() {
        download_file(LIBKRUNFW_PREBUILT_URL, &tarball_path)
            .unwrap_or_else(|e| panic!("Failed to download libkrunfw: {}", e));

        verify_sha256(&tarball_path, LIBKRUNFW_SHA256)
            .unwrap_or_else(|e| panic!("Failed to verify libkrunfw checksum: {}", e));
    }

    // Extract
    if extract_dir.exists() {
        fs::remove_dir_all(&extract_dir).ok();
    }
    extract_tarball(&tarball_path, &extract_dir)
        .unwrap_or_else(|e| panic!("Failed to extract libkrunfw: {}", e));

    println!("cargo:warning=Extracted libkrunfw to {}", src_dir.display());
    src_dir
}

/// Builds libkrunfw from the prebuilt source.
#[cfg(target_os = "macos")]
fn build_libkrunfw_macos(src_dir: &Path, install_dir: &Path) {
    build_with_make(src_dir, install_dir, "libkrunfw", HashMap::new());
}

/// Applies the cross-compilation patch to libkrun from vendored patch file.
/// Copy from https://github.com/slp/homebrew-krun
#[cfg(target_os = "macos")]
fn apply_libkrun_patch(src_dir: &Path, manifest_dir: &Path) {
    let patch_path = manifest_dir.join(LIBKRUN_PATCH_FILE);
    let patch_marker = src_dir.join(".patch_applied");

    // Skip if patch already applied
    if patch_marker.exists() {
        println!("cargo:warning=Cross-compile patch already applied");
        return;
    }

    // Verify patch file exists
    if !patch_path.exists() {
        panic!(
            "Cross-compilation patch not found at {}",
            patch_path.display()
        );
    }

    // Apply patch with git apply (works better than patch for git-style diffs)
    println!("cargo:warning=Applying cross-compilation patch...");
    let status = Command::new("git")
        .args(["apply", "--check", patch_path.to_str().unwrap()])
        .current_dir(src_dir)
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status();

    // Check if patch can be applied (might already be partially applied)
    if status.is_ok() && status.unwrap().success() {
        let status = Command::new("git")
            .args(["apply", patch_path.to_str().unwrap()])
            .current_dir(src_dir)
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .status()
            .expect("Failed to apply patch");

        if !status.success() {
            panic!("Failed to apply cross-compilation patch");
        }
        println!("cargo:warning=Cross-compilation patch applied successfully");
    } else {
        println!("cargo:warning=Patch already applied or not needed");
    }

    // Create marker file
    fs::write(&patch_marker, "applied").ok();
}

/// Builds libkrun from vendored source with cross-compilation support.
#[cfg(target_os = "macos")]
fn build_libkrun_macos(
    src_dir: &Path,
    install_dir: &Path,
    libkrunfw_install: &Path,
    manifest_dir: &Path,
) {
    // Apply cross-compilation patch from vendored patch file
    apply_libkrun_patch(src_dir, manifest_dir);

    // Build with common helper using shared build environment
    build_with_make(
        src_dir,
        install_dir,
        "libkrun",
        libkrun_build_env(libkrunfw_install),
    );
}

/// Fixes install names and re-signs libraries in a directory.
#[cfg(target_os = "macos")]
fn fix_macos_libs(lib_dir: &Path, lib_prefix: &str) -> Result<(), String> {
    for entry in fs::read_dir(lib_dir).map_err(|e| format!("Failed to read lib dir: {}", e))? {
        let entry = entry.map_err(|e| format!("Failed to read entry: {}", e))?;
        let path = entry.path();
        let filename = path.file_name().unwrap().to_string_lossy().to_string();

        if filename.starts_with(lib_prefix) && filename.contains(".dylib") {
            let metadata = fs::symlink_metadata(&path)
                .map_err(|e| format!("Failed to get metadata: {}", e))?;

            // Skip symlinks
            if metadata.file_type().is_symlink() {
                continue;
            }

            // Fix install_name to use @rpath
            fix_install_name(&filename, &path);

            // Re-sign after modifying
            let sign_status = Command::new("codesign")
                .args(["-s", "-", "--force"])
                .arg(&path)
                .status()
                .map_err(|e| format!("Failed to run codesign: {}", e))?;

            if !sign_status.success() {
                return Err(format!("codesign failed for {}", filename));
            }

            println!("cargo:warning=Fixed and signed {}", filename);
        }
    }

    Ok(())
}

/// macOS: Build libkrun and libkrunfw from source
#[cfg(target_os = "macos")]
fn build() {
    println!("cargo:warning=Building libkrun-sys for macOS (from source)");

    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());

    // Verify vendored libkrun source exists (libkrunfw is downloaded as prebuilt)
    verify_vendored_sources(&manifest_dir, false);

    let libkrun_src = manifest_dir.join("vendor/libkrun");

    // 1. Download and extract prebuilt libkrunfw
    let libkrunfw_src = download_libkrunfw_prebuilt(&out_dir);

    // 2. Build libkrunfw
    let libkrunfw_install = out_dir.join("libkrunfw");
    build_libkrunfw_macos(&libkrunfw_src, &libkrunfw_install);

    // 3. Build libkrun from vendored source (with cross-compile patch)
    let libkrun_install = out_dir.join("libkrun");
    build_libkrun_macos(
        &libkrun_src,
        &libkrun_install,
        &libkrunfw_install,
        &manifest_dir,
    );

    // 4. Fix install names for @rpath
    let libkrunfw_lib = libkrunfw_install.join(LIB_DIR);
    let libkrun_lib = libkrun_install.join(LIB_DIR);

    fix_macos_libs(&libkrunfw_lib, "libkrunfw")
        .unwrap_or_else(|e| panic!("Failed to fix libkrunfw: {}", e));

    fix_macos_libs(&libkrun_lib, "libkrun")
        .unwrap_or_else(|e| panic!("Failed to fix libkrun: {}", e));

    // 5. Configure linking
    configure_linking(&libkrun_lib, &libkrunfw_lib);
}

/// Linux: Build libkrun and libkrunfw from source
#[cfg(target_os = "linux")]
fn build() {
    println!("cargo:warning=Building libkrun-sys for Linux (from source)");

    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());

    // Verify vendored sources exist (Linux builds both from source)
    verify_vendored_sources(&manifest_dir, true);

    let libkrunfw_src = manifest_dir.join("vendor/libkrunfw");
    let libkrun_src = manifest_dir.join("vendor/libkrun");

    // Build libkrunfw first (libkrun depends on it)
    let libkrunfw_install = out_dir.join("libkrunfw");
    build_with_make(
        &libkrunfw_src,
        &libkrunfw_install,
        "libkrunfw",
        HashMap::new(),
    );

    // Build libkrun with shared build environment
    let libkrun_install = out_dir.join("libkrun");
    build_with_make(
        &libkrun_src,
        &libkrun_install,
        "libkrun",
        libkrun_build_env(&libkrunfw_install),
    );

    // Fix library names
    let libkrun_lib_dir = libkrun_install.join(LIB_DIR);
    fix_linux_libs(&libkrun_lib_dir, "libkrun")
        .unwrap_or_else(|e| panic!("Failed to fix libkrun: {}", e));

    let libkrunfw_lib_dir = libkrunfw_install.join(LIB_DIR);
    fix_linux_libs(&libkrunfw_lib_dir, "libkrunfw")
        .unwrap_or_else(|e| panic!("Failed to fix libkrunfw: {}", e));

    configure_linking(&libkrun_lib_dir, &libkrunfw_lib_dir);
}
