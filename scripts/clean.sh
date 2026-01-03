#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
source "$SCRIPT_DIR/common.sh"
source "$SCRIPT_DIR/util.sh"

# Parse command-line arguments
parse_args() {
    MODE=""
    KEEP_GUEST_BIN=""

    while [[ $# -gt 0 ]]; do
        case $1 in
            --mode)
                MODE="$2"
                shift 2
                ;;
            --keep-guest-bin)
                KEEP_GUEST_BIN=1
                shift 1
                ;;
            *)
                echo "Unknown option: $1"
                echo "Usage: $0 [--keep-guest-bin] [--mode all|dist]"
                exit 1
                ;;
        esac
    done

    # Validate mode value
    case "$MODE" in
        all|dist)
            ;;
        *)
            echo "Invalid mode: $MODE"
            echo "Valid modes: all, dist"
            exit 1
            ;;
    esac
}

parse_args "$@"

clean_cargo() {
    print_section "Cleaning Rust artifacts..."
    cargo clean
    [ -d "guest" ] && (cd guest && cargo clean)
}

clean_python() {
    print_section "Cleaning Python SDK artifacts..."
    rm -rf sdks/python/{.dylibs,.libs,bin,__pycache__,*.so,*.egg-info,build,dist,target}
    rm -rf sdks/python/boxlite/{runtime,*.so,__pycache__}
    rm -rf sdks/python/.{pytest_cache,mypy_cache,ruff_cache,coverage}*
}

clean_c() {
    print_section "Cleaning C SDK artifacts..."
    rm -rf sdks/c/{target,dist,Cargo.lock}
    rm -rf examples/c/build examples/c/execute
}

clean_node() {
    print_section "Cleaning Node.js SDK artifacts..."
    rm -rf sdks/node/dist
    rm -rf sdks/node/npm
    rm -rf sdks/node/packages
    rm -rf sdks/node/*.node
    rm -rf sdks/node/node_modules
}

clean_python_cache() {
    print_section "Cleaning Python cache files..."
    # Single find pass: remove __pycache__ dirs and .pyc/.pyo files
    find . \( -type d -name "__pycache__" -o -type f -name "*.py[co]" \) \
        -exec rm -rf {} + 2>/dev/null || true
}

clean_wheels() {
    print_section "Cleaning wheel artifacts..."
    rm -rf target/wheels
    # Note: wheelhouse is cleaned in clean_dist() to avoid duplication
}

clean_temp() {
    print_section "Cleaning temp files..."
    rm -rf .tmp tmp *.log
    # Single find pass for all temp file patterns (much faster than multiple finds)
    find . \( \
        -name ".DS_Store" -o \
        -name "*.swp" -o \
        -name "*.swo" -o \
        -name ".*.swp" -o \
        -name "*.orig" -o \
        -name "*.bak" -o \
        -name "*~" \
    \) -delete 2>/dev/null || true
    # dSYM directories need separate handling
    find . -type d -name "*.dSYM" -exec rm -rf {} + 2>/dev/null || true
}

clean_venv() {
    print_section "Removing virtual environments..."
    rm -rf .venv examples/*/.venv
}

clean_dist() {
    print_section "Cleaning distribution artifacts..."
    rm -rf dist/ include/boxlite.h
    rm -rf sdks/c/dist/
    rm -rf wheelhouse/
}

clean_runtime() {
    print_section "Cleaning runtime artifacts..."
    rm -rf target/boxlite-runtime \
           target/release/boxlite-shim \
           target/debug/boxlite-shim
    if [ -z "${KEEP_GUEST_BIN:-}" ]; then
        rm -rf target/$GUEST_TARGET/release/boxlite-guest \
               target/$GUEST_TARGET/debug/boxlite-guest
    else
        print_info "Keeping guest binary as requested"
    fi
}

main() {
    print_header "🧹 Cleaning mode=$MODE"

    if [ "$MODE" = "all" ]; then
        clean_runtime
        clean_venv
        clean_cargo
        clean_python
        clean_c
        clean_node
        clean_python_cache
        clean_wheels
        clean_temp
        clean_dist
    elif [ "$MODE" = "dist" ]; then
        clean_dist
    fi

    print_success "Cleanup complete!"
}

main "$@"
