#!/bin/bash
# Build boxlite-runtime directory with all binaries and libraries
#
# This script creates a complete runtime directory that contains everything
# needed to run BoxLite: shim binary, guest binary, and all FFI libraries.
#
# Usage:
#   ./build-runtime.sh [--dest-dir DIR] [--profile PROFILE]
#
# Options:
#   --dest-dir DIR      Destination directory (default: target/release/boxlite-runtime)
#   --profile PROFILE   Build profile: release or debug (default: release)
#
# The runtime directory will contain:
#   - boxlite-shim      VM subprocess runner
#   - boxlite-guest     Guest agent (Linux binary)
#   - libkrun.*         libkrun library
#   - libkrunfw.*       libkrunfw library
#   - libgvproxy.*      gvproxy library (if gvproxy-backend feature enabled)

set -e

# Load common utilities
SCRIPT_BUILD_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SCRIPT_DIR="$(cd "$SCRIPT_BUILD_DIR/.." && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
source "$SCRIPT_DIR/common.sh"

# Capture original working directory before any cd commands
ORIG_DIR="$(pwd)"

# Print help message
print_help() {
    cat <<EOF
Usage: build-runtime.sh [OPTIONS]

Build boxlite-runtime directory with all binaries and libraries.

Options:
  --dest-dir DIR      Destination directory (default: target/boxlite-runtime)
  --profile PROFILE   Build profile: release or debug (default: release)
  --libs-dir DIR      Directory containing FFI libraries (if not provided, will build and collect)
  --help, -h          Show this help message

The runtime directory will contain:
  - boxlite-shim      VM subprocess runner
  - boxlite-guest     Guest agent (Linux binary)
  - libkrun.*         libkrun library
  - libkrunfw.*       libkrunfw library
  - libgvproxy.*      gvproxy library (if available)

Examples:
  # Build release runtime in default location
  ./build-runtime.sh

  # Build debug runtime
  ./build-runtime.sh --profile debug

  # Build runtime in custom directory
  ./build-runtime.sh --dest-dir /tmp/my-runtime

  # Build runtime with pre-collected libraries
  ./build-runtime.sh --libs-dir /path/to/libs

  # Full workflow
  bash scripts/build/build-guest.sh
  bash scripts/build/build-shim.sh
  ./build-runtime.sh

EOF
}

# Parse command-line arguments
parse_args() {
    DEST_DIR_ARG=""
    PROFILE="release"
    LIBS_DIR_ARG=""

    while [[ $# -gt 0 ]]; do
        case $1 in
            --dest-dir)
                DEST_DIR_ARG="$2"
                shift 2
                ;;
            --profile)
                PROFILE="$2"
                shift 2
                ;;
            --libs-dir)
                LIBS_DIR_ARG="$2"
                shift 2
                ;;
            --help|-h)
                print_help
                exit 0
                ;;
            *)
                echo "Unknown option: $1"
                echo "Run with --help for usage information"
                exit 1
                ;;
        esac
    done

    # Validate PROFILE value
    if [ "$PROFILE" != "release" ] && [ "$PROFILE" != "debug" ]; then
        echo "Invalid profile: $PROFILE"
        echo "Run with --profile release or --profile debug"
        exit 1
    fi

    # Set default destination if not provided
    if [ -z "$DEST_DIR_ARG" ]; then
        DEST_DIR="$PROJECT_ROOT/target/boxlite-runtime"
    else
        # Resolve destination path to absolute path
        if [[ "$DEST_DIR_ARG" != /* ]]; then
            DEST_DIR="$ORIG_DIR/$DEST_DIR_ARG"
        else
            DEST_DIR="$DEST_DIR_ARG"
        fi
    fi

    # Resolve libs directory if provided
    if [ -n "$LIBS_DIR_ARG" ]; then
        if [[ "$LIBS_DIR_ARG" != /* ]]; then
            LIBS_DIR="$ORIG_DIR/$LIBS_DIR_ARG"
        else
            LIBS_DIR="$LIBS_DIR_ARG"
        fi
    else
        LIBS_DIR=""
    fi
}

# Detect OS
detect_platform() {
    OS=$(detect_os)
    echo "🖥️  Platform: $OS"
}

# Build boxlite-shim binary
build_shim() {
    echo ""
    print_section "Building boxlite-shim binary..."

    # Always build to ensure freshness (Cargo handles incremental compilation)
    bash "$SCRIPT_BUILD_DIR/build-shim.sh" --profile "$PROFILE"

    local shim_path="$PROJECT_ROOT/target/$PROFILE/boxlite-shim"
    if [ -f "$shim_path" ]; then
        SHIM_BINARY="$shim_path"
        print_success "Built: $shim_path"
    else
        print_error "Failed to build boxlite-shim"
        exit 1
    fi
}

# Build boxlite-guest binary
build_guest() {
    echo ""
    print_section "Building boxlite-guest binary..."

    # Always build to ensure freshness (Cargo handles incremental compilation)
    bash "$SCRIPT_BUILD_DIR/build-guest.sh" --profile "$PROFILE"

    source "$SCRIPT_DIR/util.sh"
    local guest_path="$PROJECT_ROOT/target/$GUEST_TARGET/$PROFILE/boxlite-guest"
    if [ -f "$guest_path" ]; then
        GUEST_BINARY="$guest_path"
        print_success "Built: $guest_path"
    else
        print_error "Failed to build boxlite-guest"
        exit 1
    fi
}

# Find and collect FFI libraries
collect_libraries() {
    # If caller provided libs directory, use it
    if [ -n "$LIBS_DIR" ]; then
        echo ""
        print_section "Using provided libraries directory..."

        if [ ! -d "$LIBS_DIR" ]; then
            print_error "Libraries directory not found: $LIBS_DIR"
            exit 1
        fi

        RUNTIME_LIBS_DIR="$LIBS_DIR"
        print_success "Using libraries from: $RUNTIME_LIBS_DIR"
        return 0
    fi

    # Otherwise, build and collect libraries
    echo ""
    print_section "Collecting FFI libraries..."

    cd "$PROJECT_ROOT"

    # Build boxlite crate to generate OUT_DIR with bundled libraries
    local build_flag=""
    if [ "$PROFILE" = "release" ]; then
        build_flag="--release"
    fi

    cargo build $build_flag --lib -p boxlite

    # Find the OUT_DIR by parsing cargo metadata
    local out_dir
    out_dir=$(cargo metadata --format-version 1 2>/dev/null | \
        grep -o '"target_directory":"[^"]*"' | \
        cut -d'"' -f4)

    if [ -z "$out_dir" ]; then
        out_dir="$PROJECT_ROOT/target"
    fi

    # Look for runtime directory in build output
    # It's created by boxlite/build.rs in OUT_DIR/runtime
    local runtime_src=""
    runtime_src=$(find "$out_dir/$PROFILE/build/boxlite-"*/out/runtime -type d 2>/dev/null | head -1)

    if [ -z "$runtime_src" ] || [ ! -d "$runtime_src" ]; then
        print_error "Could not find runtime libraries directory"
        echo "Expected location: $out_dir/$PROFILE/build/boxlite-*/out/runtime"
        exit 1
    fi

    RUNTIME_LIBS_DIR="$runtime_src"
    print_success "Found libraries at: $RUNTIME_LIBS_DIR"
}

# Create runtime directory and copy all components
assemble_runtime() {
    echo ""
    print_section "Assembling runtime directory..."

    # Create destination directory
    mkdir -p "$DEST_DIR"

    # Copy binaries
    print_step "Copying boxlite-shim... "
    cp "$SHIM_BINARY" "$DEST_DIR/"
    echo "✓"

    print_step "Copying boxlite-guest... "
    cp "$GUEST_BINARY" "$DEST_DIR/"
    echo "✓"

    # Copy all libraries (preserve symlinks)
    print_step "Copying libraries... "
    cp -P "$RUNTIME_LIBS_DIR"/* "$DEST_DIR/" 2>/dev/null || true
    echo "✓"

    # Sign shim on macOS (always, to ensure proper entitlements)
    if [ "$OS" = "macos" ] && [ -f "$DEST_DIR/boxlite-shim" ]; then
        echo ""
        print_section "Signing boxlite-shim... "
        "$SCRIPT_BUILD_DIR/sign.sh" "$DEST_DIR/boxlite-shim"
        echo "✓"
    fi

    print_success "Runtime directory assembled"
}

# Display runtime directory contents
show_summary() {
    echo ""
    print_section "Runtime Directory Summary"
    echo "Location: $DEST_DIR"
    echo ""
    echo "Contents:"
    ls -lh "$DEST_DIR" | tail -n +2 | while read -r line; do
        echo "  $line"
    done
    echo ""

    # Show file types
    echo "File types:"
    for file in "$DEST_DIR"/*; do
        if [ -f "$file" ]; then
            local filename
            local filetype
            filename=$(basename "$file")
            filetype=$(file "$file" | cut -d: -f2-)
            echo "  $filename:$filetype"
        fi
    done
}

# Main execution
main() {
    parse_args "$@"

    print_header "🔨 BoxLite Runtime Preparation"
    echo "Profile: $PROFILE"
    echo "Destination: $DEST_DIR"
    echo ""

    detect_platform
    build_shim
    build_guest
    collect_libraries
    assemble_runtime
    show_summary

    echo ""
    print_success "✅ Runtime preparation complete!"
    echo ""
}

main "$@"
