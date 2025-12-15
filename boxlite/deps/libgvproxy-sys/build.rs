use std::env;
use std::fs;
use std::path::Path;
use std::process::Command;

/// Builds libgvproxy from Go sources using cgo.
///
/// Steps:
/// 1. Downloads Go module dependencies
/// 2. Compiles Go code as a C shared library
/// 3. Fixes the install_name for proper loading
fn build_gvproxy(source_dir: &Path, output_path: &Path) {
    println!("cargo:warning=Building libgvproxy from Go sources...");

    // Download Go dependencies
    let download_status = Command::new("go")
        .args(["mod", "download"])
        .current_dir(source_dir)
        .status()
        .expect("Failed to run 'go mod download' - ensure Go is installed");

    if !download_status.success() {
        panic!("Failed to download Go module dependencies");
    }

    // Build as C shared library
    let mut build_cmd = Command::new("go");
    build_cmd.args(["build", "-buildmode=c-shared"]);

    // macOS: Reserve space for install_name_tool to modify paths later
    #[cfg(target_os = "macos")]
    build_cmd.arg("-ldflags=-extldflags=-headerpad_max_install_names");

    build_cmd.args([
        "-o",
        output_path.to_str().expect("Invalid output path"),
        "main.go",
        "stats.go",
    ]);

    let build_status = build_cmd
        .current_dir(source_dir)
        .status()
        .expect("Failed to run 'go build' - ensure Go is installed");

    if !build_status.success() {
        panic!("Failed to build libgvproxy");
    }

    println!("cargo:warning=Successfully built libgvproxy");
}

/// Fixes the install_name on macOS to use an absolute path.
/// This allows install_name_tool to modify the library path during wheel repair.
#[cfg(target_os = "macos")]
fn fix_install_name(lib_name: &str, lib_path: &Path) {
    let lib_path_str = lib_path.to_str().expect("Invalid library path");

    let status = Command::new("install_name_tool")
        .args(["-id", &format!("@rpath/{}", lib_name), lib_path_str])
        .status()
        .expect("Failed to execute install_name_tool");

    if !status.success() {
        panic!("Failed to set install_name for libgvproxy");
    }
}

#[cfg(target_os = "linux")]
fn fix_install_name(lib_name: &str, lib_path: &Path) {
    let lib_path_str = lib_path.to_str().expect("Invalid library path");

    let status = Command::new("patchelf")
        .args([
            "--set-soname",
            lib_name, // On Linux, SONAME is just the library name, not @rpath/name
            lib_path_str,
        ])
        .status()
        .expect("Failed to execute patchelf");

    if !status.success() {
        panic!("Failed to set install_name for libgvproxy");
    }
}

/// Determines the library filename based on the target platform.
fn get_library_name() -> &'static str {
    if cfg!(target_os = "macos") {
        "libgvproxy.dylib"
    } else if cfg!(target_os = "linux") {
        "libgvproxy.so"
    } else if cfg!(target_os = "windows") {
        "libgvproxy.dll"
    } else {
        panic!("Unsupported platform for libgvproxy");
    }
}

fn main() {
    // Rebuild if Go sources change
    println!("cargo:rerun-if-changed=gvproxy-bridge/main.go");
    println!("cargo:rerun-if-changed=gvproxy-bridge/stats.go");
    println!("cargo:rerun-if-changed=gvproxy-bridge/go.mod");

    // Check for stub mode (for CI linting without building)
    // Set BOXLITE_DEPS_STUB=1 to skip building and emit stub link directives
    if env::var("BOXLITE_DEPS_STUB").is_ok() {
        println!("cargo:warning=BOXLITE_DEPS_STUB mode: skipping libgvproxy build");
        println!("cargo:rustc-link-lib=dylib=gvproxy");
        println!("cargo:LIBGVPROXY_BOXLITE_DEP=/nonexistent");
        return;
    }

    let out_dir = env::var("OUT_DIR").expect("OUT_DIR not set");
    let manifest_dir = env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR not set");

    let source_dir = Path::new(&manifest_dir).join("gvproxy-bridge");
    let lib_name = get_library_name();
    let lib_output = Path::new(&out_dir).join(lib_name);

    // Build libgvproxy from Go sources
    build_gvproxy(&source_dir, &lib_output);

    // Fix install_name on use absolute path
    fix_install_name(lib_name, &lib_output);

    // Copy header file for downstream C/C++ usage (optional)
    let header_src = source_dir.join("libgvproxy.h");
    if header_src.exists() {
        let header_dst = Path::new(&out_dir).join("libgvproxy.h");
        fs::copy(&header_src, &header_dst).expect("Failed to copy libgvproxy.h");
    }

    // Tell Cargo where to find the library
    println!("cargo:rustc-link-search=native={}", out_dir);
    println!("cargo:rustc-link-lib=dylib=gvproxy");

    // Expose library directory to downstream crates (used by boxlite/build.rs)
    // Convention: {LIBNAME}_BOXLITE_DEP=<path> for auto-discovery
    println!("cargo:LIBGVPROXY_BOXLITE_DEP={}", out_dir);
}
