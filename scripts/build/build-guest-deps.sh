#!/bin/bash
# Build and publish static e2fsprogs tools alongside the musl guest binary.
# Usage:
#   build-guest-deps.sh [--target TARGET] [--profile release|debug]

set -euo pipefail

die() {
    echo "ERROR: $*" >&2
    return 1
}

portable_mode() {
    if stat -c '%a' "$1" >/dev/null 2>&1; then
        stat -c '%a' "$1"
    else
        stat -f '%Lp' "$1"
    fi
}

host_arch() {
    case "$(uname -m)" in
        x86_64|amd64) echo x86_64 ;;
        arm64|aarch64) echo aarch64 ;;
        *) echo unsupported ;;
    esac
}

target_arch() {
    case "$1" in
        x86_64-unknown-linux-musl) echo x86_64 ;;
        aarch64-unknown-linux-musl) echo aarch64 ;;
        *) die "unsupported guest tools target: $1"; return 2 ;;
    esac
}

musl_cc() {
    local arch="$1" compiler
    compiler=$(command -v "${arch}-linux-musl-gcc" 2>/dev/null || true)
    if [ -z "$compiler" ] && [ "$arch" = "$(host_arch)" ]; then
        compiler=$(command -v musl-gcc 2>/dev/null || true)
    fi
    [ -n "$compiler" ] || {
        die "musl compiler not found for $arch"
        return 1
    }
    echo "$compiler"
}

artifact_exists() {
    [ -e "$1" ] || [ -L "$1" ]
}

remove_tree() {
    local path="$1" label="$2"
    [ -n "$path" ] || return 0
    [ -e "$path" ] || [ -L "$path" ] || return 0
    chmod -R u+w "$path" 2>/dev/null || true
    if ! rm -rf -- "$path"; then
        die "failed to remove $label: $path"
        return 1
    fi
}

cleanup() {
    local status=$? cleanup_failed=0
    trap - EXIT HUP INT TERM
    set +e
    if [ "$status" -ne 0 ] && [ -n "$work" ] && [ -s "$work/build.log" ]; then
        echo "ERROR: guest e2fsprogs build failed; build output follows:" >&2
        cat "$work/build.log" >&2
    fi
    remove_tree "$work" "temporary guest tools work directory" || cleanup_failed=1
    remove_tree "$stage" "guest tools staging directory" || cleanup_failed=1
    if [ "$status" -eq 0 ] && [ "$cleanup_failed" -ne 0 ]; then
        status=1
    fi
    exit "$status"
}

parse_args() {
    target="${GUEST_TARGET:-}"
    profile="${PROFILE:-release}"

    while [ "$#" -gt 0 ]; do
        case "$1" in
            --target)
                [ "$#" -ge 2 ] || { die "--target requires a value"; return 2; }
                target="$2"
                shift 2
                ;;
            --profile)
                [ "$#" -ge 2 ] || { die "--profile requires a value"; return 2; }
                profile="$2"
                shift 2
                ;;
            --help|-h)
                echo "Usage: $0 [--target TARGET] [--profile release|debug]"
                exit 0
                ;;
            *)
                die "unknown option: $1"
                return 2
                ;;
        esac
    done

    if [ -z "$target" ]; then
        target=$(bash "$root/scripts/util.sh" --target)
    fi
    arch=$(target_arch "$target") || return $?
    case "$profile" in
        release|debug) ;;
        *) die "unsupported profile: $profile"; return 2 ;;
    esac
}

verify_tool() {
    local path="$1"
    [ -f "$path" ] && [ ! -L "$path" ] && [ -s "$path" ] || {
        die "invalid guest tool: $path"
        return 1
    }
    [ "$(portable_mode "$path")" = 755 ] || {
        die "guest tool must have mode 0755: $path"
        return 1
    }
    bash "$root/scripts/util.sh" --verify-guest-elf "$target" "$path"
}

verify_stage() {
    local actual expected
    verify_tool "$stage/mke2fs"
    verify_tool "$stage/resize2fs"
    actual=$(find "$stage" ! -path "$stage" -prune -print | LC_ALL=C sort) || {
        die "failed to inspect guest tool staging: $stage"
        return 1
    }
    expected=$(printf '%s\n' "$stage/mke2fs" "$stage/resize2fs" | LC_ALL=C sort)
    [ "$actual" = "$expected" ] || {
        die "guest tool staging must contain exactly mke2fs and resize2fs"
        return 1
    }
}

preflight_final_paths() {
    local name final
    for name in mke2fs resize2fs; do
        final="$output_parent/$name"
        if artifact_exists "$final" && { [ ! -f "$final" ] || [ -L "$final" ]; }; then
            die "refusing to replace non-regular guest tool: $final"
            return 1
        fi
    done
}

publish_tools() {
    local name final
    preflight_final_paths
    for name in mke2fs resize2fs; do
        final="$output_parent/$name"
        mv -- "$stage/$name" "$final"
        verify_tool "$final"
    done
}

build_tools() {
    local source="$root/src/deps/e2fsprogs-sys/vendor/e2fsprogs"
    local util="$root/scripts/util.sh"
    [ -x "$source/configure" ] || {
        die "e2fsprogs submodule is not initialized at $source"
        return 1
    }
    [ -f "$util" ] || { die "missing build utility: $util"; return 1; }

    output_parent="$root/target/$target/$profile"
    mkdir -p "$output_parent"
    [ -d "$output_parent" ] && [ ! -L "$output_parent" ] || {
        die "guest artifact directory must be a regular directory: $output_parent"
        return 1
    }

    local cc build_cc cc_name build_cc_name headers host jobs cflags ldflags
    cc=$(musl_cc "$arch")
    build_cc=$(command -v "${BUILD_CC:-cc}") || { die "host C compiler not found"; return 1; }
    cc="$(cd "$(dirname "$cc")" && pwd -P)/$(basename "$cc")"
    build_cc="$(cd "$(dirname "$build_cc")" && pwd -P)/$(basename "$build_cc")"
    cc_name=$(basename "$cc")
    build_cc_name=$(basename "$build_cc")

    headers=$(bash "$util" --ensure-linux-headers "$arch")
    [ -f "$headers/asm/unistd.h" ] && [ -f "$headers/linux/audit.h" ] || {
        die "Linux headers are incomplete for $arch"
        return 1
    }
    headers=$(cd "$headers" && pwd -P)

    if [ "$profile" = release ]; then
        cflags='-O2 -fno-pie -ffunction-sections -fdata-sections'
        ldflags='-no-pie -Wl,--gc-sections'
    else
        cflags='-O0 -g3 -fno-omit-frame-pointer -fno-pie'
        ldflags='-no-pie'
    fi

    host="${target/-unknown/}"
    jobs=$(getconf _NPROCESSORS_ONLN 2>/dev/null || echo 4)
    # e2fsprogs' generated configure and Makefiles do not preserve source,
    # compiler, or include paths containing shell-special characters. Build
    # through controlled aliases, then stage only the two requested files.
    work=$(mktemp -d "${TMPDIR:-/tmp}/boxlite-e2fsprogs-work.XXXXXX")
    mkdir -p "$work/build" "$work/bin/target" "$work/bin/build"
    ln -s "$source" "$work/source"
    ln -s "$headers" "$work/headers"
    ln -s "$cc" "$work/bin/target/$cc_name"
    ln -s "$build_cc" "$work/bin/build/$build_cc_name"
    source="$work/source"
    headers="$work/headers"
    cc="$work/bin/target/$cc_name"
    build_cc="$work/bin/build/$build_cc_name"

    local -a configure_args=("--build=$("$source/config/config.guess")" "--host=$host" --prefix=/usr
        --enable-libuuid --enable-libblkid --enable-resizer --disable-elf-shlibs
        --disable-bsd-shlibs --disable-hardening --disable-debugfs --disable-imager
        --disable-defrag --disable-fsck --disable-e2initrd-helper --disable-uuidd
        --disable-tdb --disable-nls --disable-rpath --disable-fuse2fs --disable-backtrace
        --disable-tls --without-pthread --without-libarchive)

    echo "🔨 Building static e2fsprogs tools for $target ($profile)..."
    (
        cd "$work/build"
        env BUILD_CC="$build_cc" BUILD_CFLAGS="${BUILD_CFLAGS:-}" \
            BUILD_LDFLAGS="${BUILD_LDFLAGS:-}" CC="$cc" PKG_CONFIG=false \
            CPPFLAGS="-I$headers" CFLAGS="$cflags" CFLAGS_STLIB="$cflags" \
            LDFLAGS="$ldflags" LDFLAGS_STATIC="$ldflags -static" \
            "$source/configure" "${configure_args[@]}"
        make -j"$jobs" libs
        make -C misc -j"$jobs" mke2fs.static
        make -C resize -j"$jobs" resize2fs.static
    ) >"$work/build.log" 2>&1

    stage=$(mktemp -d "$output_parent/.guest-tools-stage.XXXXXX")
    install -m 0755 "$work/build/misc/mke2fs.static" "$stage/mke2fs"
    install -m 0755 "$work/build/resize/resize2fs.static" "$stage/resize2fs"
    verify_stage
    publish_tools
    echo "✅ Guest e2fsprogs tools built: $output_parent/{mke2fs,resize2fs}"
}

script_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)
root=$(cd "$script_dir/../.." && pwd -P)
target=""
profile=""
arch=""
work=""
output_parent=""
stage=""

parse_args "$@"

trap cleanup EXIT
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM
build_tools
