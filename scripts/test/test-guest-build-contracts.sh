#!/bin/bash
# Exercise build-entrypoint contracts that are awkward to prove from artifact inspection alone.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd -P)"
target=""
profile=release
test_case=all
while [ "$#" -gt 0 ]; do
    case "$1" in
        --target) [ "$#" -ge 2 ] || exit 2; target="$2"; shift 2 ;;
        --profile) [ "$#" -ge 2 ] || exit 2; profile="$2"; shift 2 ;;
        --case) [ "$#" -ge 2 ] || exit 2; test_case="$2"; shift 2 ;;
        --help|-h)
            echo "Usage: $0 [--target TARGET] [--profile release|debug] [--case CASE]"
            exit 0
            ;;
        *) echo "ERROR: unknown option: $1" >&2; exit 2 ;;
    esac
done
case "$profile" in release|debug) ;; *) echo "ERROR: unsupported profile: $profile" >&2; exit 2 ;; esac
case "$test_case" in
    all|build-output|cleanup|failure-leaves-history-untouched|guest-mode|historical-content|missing-readelf|unsafe-guest-symlink) ;;
    *) echo "ERROR: unsupported guest build contract case: $test_case" >&2; exit 2 ;;
esac
if [ -z "$target" ]; then target=$(bash "$root/scripts/util.sh" --target); fi
case "$target" in
    x86_64-unknown-linux-musl|aarch64-unknown-linux-musl) ;;
    *) echo "ERROR: unsupported target: $target" >&2; exit 2 ;;
esac

output_parent="$root/target/$target/$profile"
guest="$output_parent/boxlite-guest"
legacy_rootfs="$output_parent/guest-rootfs"
legacy_guest_tools="$output_parent/guest-tools"
legacy_rootfs_sentinel="$legacy_rootfs/.guest-build-contract-sentinel.$$"
legacy_guest_tools_sentinel="$legacy_guest_tools/.guest-build-contract-sentinel.$$"
profile_sentinel="$output_parent/.guest-build-contract-sentinel.$$"
legacy_rootfs_content="historical guest-rootfs content must survive"
legacy_guest_tools_content="historical guest-tools content must survive"
profile_content="unrelated profile content must survive"
legacy_rootfs_created=0
legacy_guest_tools_created=0
saved_guest=""
tmp=$(mktemp -d "${TMPDIR:-/tmp}/boxlite-guest-build-contracts.XXXXXX")

cleanup_test() {
    local status=$?
    trap - EXIT HUP INT TERM
    set +e
    rm -f -- "$profile_sentinel" "$legacy_rootfs_sentinel" "$legacy_guest_tools_sentinel"
    if [ "$legacy_rootfs_created" -eq 1 ]; then rmdir "$legacy_rootfs" 2>/dev/null || true; fi
    if [ "$legacy_guest_tools_created" -eq 1 ]; then rmdir "$legacy_guest_tools" 2>/dev/null || true; fi
    if [ -n "$saved_guest" ] && [ -f "$saved_guest" ]; then
        rm -f -- "$guest"
        mv -- "$saved_guest" "$guest"
        saved_guest=""
    fi
    if [ -f "$guest" ] && [ ! -L "$guest" ]; then chmod 0755 "$guest" 2>/dev/null || true; fi
    if ! rm -rf -- "$tmp"; then
        echo "ERROR: failed to remove test directory: $tmp" >&2
        if [ "$status" -eq 0 ]; then status=1; fi
    fi
    exit "$status"
}
trap cleanup_test EXIT
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM

fail() { echo "ERROR: $*" >&2; exit 1; }
mode() { if stat -c '%a' "$1" >/dev/null 2>&1; then stat -c '%a' "$1"; else stat -f '%Lp' "$1"; fi; }

assert_sentinel() {
    local path="$1" expected="$2" label="$3"
    [ -f "$path" ] && [ ! -L "$path" ] || fail "$label was removed or replaced"
    [ "$(sed -n '1p' "$path")" = "$expected" ] || fail "$label was modified"
}

prepare_historical_sentinel() {
    local directory="$1" path="$2" content="$3" created_variable="$4"
    if [ -e "$directory" ] || [ -L "$directory" ]; then
        [ -d "$directory" ] && [ ! -L "$directory" ] || fail "unsafe historical artifact directory: $directory"
    else
        mkdir -p "$directory"
        printf -v "$created_variable" '%s' 1
    fi
    printf '%s\n' "$content" > "$path"
}

prepare_history() {
    mkdir -p "$output_parent"
    printf '%s\n' "$profile_content" > "$profile_sentinel"
    prepare_historical_sentinel "$legacy_rootfs" "$legacy_rootfs_sentinel" \
        "$legacy_rootfs_content" legacy_rootfs_created
    prepare_historical_sentinel "$legacy_guest_tools" "$legacy_guest_tools_sentinel" \
        "$legacy_guest_tools_content" legacy_guest_tools_created
}

assert_history() {
    local suffix="${1:-}"
    assert_sentinel "$profile_sentinel" "$profile_content" "unrelated profile content$suffix"
    assert_sentinel "$legacy_guest_tools_sentinel" "$legacy_guest_tools_content" \
        "historical guest-tools content$suffix"
    assert_sentinel "$legacy_rootfs_sentinel" "$legacy_rootfs_content" \
        "historical guest-rootfs content$suffix"
}

build_guest_binary() {
    bash "$root/scripts/build/build-guest.sh" --target "$target" --profile "$profile"
}

prepare_stub_toolchain() {
    stub_bin="$tmp/incremental-bin"
    stub_cache="$tmp/incremental-cache"
    mkdir -p "$stub_bin" "$stub_cache/libseccomp/$target/2.5.5/lib"
    : > "$stub_cache/libseccomp/$target/2.5.5/lib/libseccomp.a"
    printf '%s\n' '#!/bin/sh' 'exit 0' > "$stub_bin/cargo"
    printf '%s\n' '#!/bin/sh' 'exit 0' > "$stub_bin/rustc"
    printf '%s\n' '#!/bin/sh' "echo '$target (installed)'" > "$stub_bin/rustup"
    for compiler in musl-gcc x86_64-linux-musl-gcc aarch64-linux-musl-gcc; do
        printf '%s\n' '#!/bin/sh' 'exit 0' > "$stub_bin/$compiler"
        chmod 0755 "$stub_bin/$compiler"
    done
    chmod 0755 "$stub_bin/cargo" "$stub_bin/rustc" "$stub_bin/rustup"
}

run_incremental_guest_build() {
    prepare_stub_toolchain
    BASH_ENV="${BASH_ENV:-}" BOXLITE_CACHE="$stub_cache" PATH="$stub_bin:$PATH" \
        bash "$root/scripts/build/build-guest.sh" --target "$target" --profile "$profile"
}

ensure_guest_binary() {
    if [ ! -f "$guest" ] || [ -L "$guest" ] || [ ! -s "$guest" ]; then
        build_guest_binary
    fi
}

check_historical_content() {
    prepare_history
    make --no-print-directory -C "$root" guest GUEST_TARGET="$target" PROFILE="$profile"
    assert_history " after successful guest build"
}

check_failure_leaves_history_untouched() {
    prepare_history
    prepare_stub_toolchain
    printf '%s\n' \
        'case "$0" in' \
        '  */scripts/build/build-guest.sh) exit 0 ;;' \
        'esac' > "$tmp/skip-guest-binary-build.bash"
    if BASH_ENV="$tmp/skip-guest-binary-build.bash" \
        PATH="$stub_bin:$PATH" BUILD_CC=boxlite-intentionally-missing-build-cc \
        make --no-print-directory -C "$root" guest GUEST_TARGET="$target" PROFILE="$profile" \
        >"$tmp/tools-failure.out" 2>"$tmp/tools-failure.err"; then
        fail "guest build unexpectedly accepted a missing host C compiler"
    fi
    grep -Fq "host C compiler not found" "$tmp/tools-failure.err" || \
        fail "guest build failed before the intended tools phase"
    assert_history " after tools failure"
}

check_guest_mode() {
    ensure_guest_binary
    chmod 0777 "$guest"
    run_incremental_guest_build
    [ "$(mode "$guest")" = 755 ] || fail "incremental guest build did not normalize mode to 0755"
}

check_missing_readelf() {
    ensure_guest_binary
    prepare_stub_toolchain
    printf '%s\n' \
        'command() {' \
        '  if [ "$1" = -v ]; then' \
        '    case "$2" in *readelf) return 1 ;; esac' \
        '  fi' \
        '  builtin command "$@"' \
        '}' > "$tmp/hide-readelf.bash"
    if BASH_ENV="$tmp/hide-readelf.bash" run_incremental_guest_build \
        >"$tmp/missing-readelf.out" 2>"$tmp/missing-readelf.err"; then
        fail "guest build accepted an unverifiable binary without readelf"
    fi
    grep -Fq "readelf or llvm-readelf is required to verify boxlite-guest" \
        "$tmp/missing-readelf.err" || fail "missing-readelf reason was not written to stderr"
}

check_unsafe_guest_symlink() {
    local symlink_target="$tmp/guest-symlink-target"
    ensure_guest_binary
    saved_guest="$tmp/original-boxlite-guest"
    mv -- "$guest" "$saved_guest"
    cp "$saved_guest" "$symlink_target"
    chmod 0644 "$symlink_target"
    ln -s "$symlink_target" "$guest"

    if run_incremental_guest_build >"$tmp/unsafe-guest.out" 2>"$tmp/unsafe-guest.err"; then
        fail "incremental guest build accepted a symlink output"
    fi
    grep -Fq "invalid boxlite-guest output" "$tmp/unsafe-guest.out" || \
        fail "symlink guest output was rejected for the wrong reason"
    [ "$(mode "$symlink_target")" = 644 ] || \
        fail "guest build changed the symlink target mode before rejecting the output"

    rm -f -- "$guest"
    mv -- "$saved_guest" "$guest"
    saved_guest=""
}

prepare_guest_tools_fixture() {
    mkdir -p "$fixture_root/scripts/build" \
        "$fixture_root/src/deps/e2fsprogs-sys/vendor/e2fsprogs/config" \
        "$fixture_bin" "$headers/asm" "$headers/linux"
    # build-guest-deps.sh resolves its own root with pwd -P, so the staging path it
    # reports follows symlinks (macOS: /var, /tmp -> /private/...). Normalize the
    # fixture root the same way so the expected path matches what the builder emits.
    fixture_root="$(cd "$fixture_root" && pwd -P)"
    fixture_target="$fixture_root/target/$target/$profile"
    cp "$root/scripts/build/build-guest-deps.sh" "$fixture_root/scripts/build/"
    printf '%s\n' '#!/bin/bash' \
        'case "$1" in' \
        '  --ensure-linux-headers) printf "%s\\n" "$CLEANUP_TEST_HEADERS" ;;' \
        '  --verify-guest-elf) exit 0 ;;' \
        '  *) exit 2 ;;' \
        'esac' > "$fixture_root/scripts/util.sh"
    printf '%s\n' '#!/bin/bash' \
        'mkdir -p misc resize' \
        'printf tool > misc/mke2fs.static' \
        'printf tool > resize/resize2fs.static' \
        'chmod 0755 misc/mke2fs.static resize/resize2fs.static' \
        > "$fixture_root/src/deps/e2fsprogs-sys/vendor/e2fsprogs/configure"
    printf '%s\n' '#!/bin/sh' 'echo x86_64-pc-linux-gnu' \
        > "$fixture_root/src/deps/e2fsprogs-sys/vendor/e2fsprogs/config/config.guess"
    printf '%s\n' '#!/bin/sh' 'exit 0' > "$fixture_bin/make"
    printf '%s\n' '#!/bin/sh' 'exit 0' > "$fixture_bin/${target%%-*}-linux-musl-gcc"
    printf '%s\n' '#!/bin/sh' 'exit 0' > "$fixture_bin/cc"
    chmod 0755 "$fixture_root/scripts/util.sh" \
        "$fixture_root/src/deps/e2fsprogs-sys/vendor/e2fsprogs/configure" \
        "$fixture_root/src/deps/e2fsprogs-sys/vendor/e2fsprogs/config/config.guess" \
        "$fixture_bin/make" "$fixture_bin/${target%%-*}-linux-musl-gcc" "$fixture_bin/cc"
    : > "$headers/asm/unistd.h"
    : > "$headers/linux/audit.h"
}

check_build_output() {
    local fixture_root="$tmp/output-repo" fixture_bin="$tmp/output-bin"
    local headers="$tmp/output-headers" fixture_target phase status expected
    prepare_guest_tools_fixture
    cat >> "$fixture_root/src/deps/e2fsprogs-sys/vendor/e2fsprogs/configure" <<'EOF'
echo 'configure stdout'
echo 'configure stderr' >&2
if [ "$FAIL_PHASE" = configure ]; then exit 41; fi
EOF
    cat > "$fixture_bin/make" <<'EOF'
#!/bin/bash
case "$*" in
    *mke2fs.static*) phase=mke2fs ;;
    *resize2fs.static*) phase=resize2fs ;;
    *) phase=libs ;;
esac
echo "$phase stdout"
echo "$phase stderr" >&2
if [ "$FAIL_PHASE" = "$phase" ]; then exit 42; fi
EOF
    for phase in success configure libs mke2fs resize2fs; do
        rm -rf -- "$fixture_target"
        status=0
        FAIL_PHASE="$phase" CLEANUP_TEST_HEADERS="$headers" BUILD_CC=cc TMPDIR="$tmp" \
            PATH="$fixture_bin:$PATH" bash "$fixture_root/scripts/build/build-guest-deps.sh" \
            --target "$target" --profile "$profile" >"$tmp/build.out" 2>"$tmp/build.err" || status=$?
        if [ "$phase" = success ]; then
            [ "$status" -eq 0 ] || fail "successful build returned $status"
            if grep -Eq '(configure|libs|mke2fs|resize2fs) (stdout|stderr)' "$tmp/build.out" "$tmp/build.err"; then
                fail "successful guest tools build leaked configure or make output"
            fi
            grep -Fq 'Guest e2fsprogs tools built:' "$tmp/build.out" || fail "missing success summary"
            [ -s "$fixture_target/mke2fs" ] && [ -s "$fixture_target/resize2fs" ] || fail "tools were not published"
            continue
        fi
        expected=42
        if [ "$phase" = configure ]; then expected=41; fi
        [ "$status" -eq "$expected" ] || fail "$phase failure returned $status instead of $expected"
        for expected in configure "$phase"; do
            grep -Fxq "$expected stdout" "$tmp/build.err" || fail "$phase failure lost $expected stdout"
            grep -Fxq "$expected stderr" "$tmp/build.err" || fail "$phase failure lost $expected stderr"
        done
        case "$phase" in
            configure) expected=libs ;;
            libs) expected=mke2fs ;;
            mke2fs) expected=resize2fs ;;
            resize2fs) expected='Guest e2fsprogs tools built:' ;;
        esac
        if grep -Fq "$expected" "$tmp/build.out" "$tmp/build.err"; then
            fail "build continued after $phase failed"
        fi
        [ ! -e "$fixture_target/mke2fs" ] && [ ! -e "$fixture_target/resize2fs" ] || fail "failed build published tools"
    done
}

check_cleanup_failure() {
    local fixture_root="$tmp/cleanup-repo" fixture_bin="$tmp/cleanup-bin"
    local headers="$tmp/cleanup-headers" cleanup_env="$tmp/fail-cleanup.bash"
    local fixture_target
    prepare_guest_tools_fixture
    printf '%s\n' \
        'rm() {' \
        '  case "$*" in' \
        '    *boxlite-e2fsprogs-work.*|*.guest-tools-stage.*) return 71 ;;' \
        '  esac' \
        '  command rm "$@"' \
        '}' > "$cleanup_env"

    if BASH_ENV="$cleanup_env" CLEANUP_TEST_HEADERS="$headers" BUILD_CC=cc TMPDIR="$tmp" \
        PATH="$fixture_bin:$PATH" bash "$fixture_root/scripts/build/build-guest-deps.sh" \
        --target "$target" --profile "$profile" >"$tmp/cleanup.out" 2>"$tmp/cleanup.err"; then
        fail "guest tools builder hid successful-build cleanup failures"
    fi
    grep -Fq "failed to remove temporary guest tools work directory" "$tmp/cleanup.err" || \
        fail "guest tools work cleanup failure was not reported"
    grep -Fq "failed to remove guest tools staging directory" "$tmp/cleanup.err" || \
        fail "guest tools stage cleanup failure was not reported"
    grep -Fq "failed to remove temporary guest tools work directory: $tmp/boxlite-e2fsprogs-work." \
        "$tmp/cleanup.err" || fail "guest tools work directory escaped the test-owned temporary root"
    grep -Fq "failed to remove guest tools staging directory: $fixture_target/.guest-tools-stage." \
        "$tmp/cleanup.err" || fail "guest tools staging escaped the test-owned temporary root"
    [ -s "$fixture_target/mke2fs" ] && [ -s "$fixture_target/resize2fs" ] || \
        fail "cleanup fixture did not publish both tools before cleanup failed"
}

case "$test_case" in
    historical-content) check_historical_content ;;
    failure-leaves-history-untouched) check_failure_leaves_history_untouched ;;
    guest-mode) check_guest_mode ;;
    missing-readelf) check_missing_readelf ;;
    unsafe-guest-symlink) check_unsafe_guest_symlink ;;
    cleanup) check_cleanup_failure ;;
    build-output) check_build_output ;;
    all)
        check_historical_content
        check_failure_leaves_history_untouched
        check_guest_mode
        check_missing_readelf
        check_unsafe_guest_symlink
        check_cleanup_failure
        check_build_output
        ;;
esac

echo "Guest build contract checks passed ($test_case)"
