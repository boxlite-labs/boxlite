//! Integration tests for LiteBox::copy_into / copy_out.
//!
//! All tests share a single VM to avoid one boot cycle per case.
//! Run with:
//!
//! ```sh
//! cargo test -p boxlite --test copy -- --nocapture
//! ```

mod common;

use boxlite::BoxliteRuntime;
use boxlite::runtime::options::BoxliteOptions;
use boxlite::{BoxCommand, CopyOptions, CopySourceKind, LiteBox};
use std::path::Path;
use tempfile::TempDir;
use tokio_stream::StreamExt;

// ============================================================================
// HELPERS
// ============================================================================

/// Exec a command inside the box and return stdout (asserts exit code 0).
async fn exec_stdout(bx: &LiteBox, cmd: BoxCommand) -> String {
    let mut execution = bx.exec(cmd).await.expect("exec failed");
    let mut stdout = String::new();
    if let Some(mut stream) = execution.stdout() {
        while let Some(chunk) = stream.next().await {
            stdout.push_str(&chunk);
        }
    }
    let result = execution.wait().await.expect("wait failed");
    assert_eq!(result.exit_code, 0, "command should exit 0");
    stdout
}

/// Exec a command and return exit code (don't assert success).
async fn exec_exit_code(bx: &LiteBox, cmd: BoxCommand) -> i32 {
    let mut execution = bx.exec(cmd).await.expect("exec failed");
    if let Some(mut stream) = execution.stdout() {
        while stream.next().await.is_some() {}
    }
    let result = execution.wait().await.expect("wait failed");
    result.exit_code
}

// ============================================================================
// SINGLE TEST ENTRY POINT — one VM, all cases
// ============================================================================

/// A single file streamed into an *existing* directory must land inside it
/// (Unix cp semantics), not try to overwrite the directory path. This is the
/// destination-side signal that the streaming path used to drop.
async fn streaming_single_file_into_existing_dir(bx: &LiteBox, tmp: &Path) {
    let host_src = tmp.join("stream-into-dir.txt");
    std::fs::write(&host_src, b"landed inside\n").unwrap();

    let (source_is_dir, tar) = boxlite_shared::tar::pack_stream(
        host_src.clone(),
        boxlite_shared::tar::PackContext {
            follow_symlinks: false,
            include_parent: false,
        },
    )
    .await
    .expect("pack_stream");
    assert!(!source_is_dir);

    // `/root` already exists as a directory; the file must go inside it.
    bx.copy_in_stream(
        tar,
        "/root",
        CopySourceKind::from_wire(Some(source_is_dir)),
        CopyOptions::default(),
    )
    .await
    .expect("copy_in_stream into existing directory");

    let out = exec_stdout(
        bx,
        BoxCommand::new("cat").args(["/root/stream-into-dir.txt"]),
    )
    .await;
    assert_eq!(out, "landed inside\n");
}

/// Exercise the streaming entry points (`copy_in_stream` / `copy_out_stream`)
/// end-to-end: pack a host file into a byte stream, stream it into the guest,
/// then stream it back out and unpack it — with no temp-file staging.
async fn streaming_roundtrip(bx: &LiteBox, tmp: &Path) {
    let host_src = tmp.join("stream-src.txt");
    std::fs::write(&host_src, b"streaming hello").unwrap();

    let (source_is_dir, tar) = boxlite_shared::tar::pack_stream(
        host_src.clone(),
        boxlite_shared::tar::PackContext {
            follow_symlinks: false,
            include_parent: false,
        },
    )
    .await
    .expect("pack_stream");
    assert!(
        !source_is_dir,
        "single file must report source_is_dir=false"
    );

    bx.copy_in_stream(
        tar,
        "/root/stream-src.txt",
        CopySourceKind::from_wire(Some(source_is_dir)),
        CopyOptions::default(),
    )
    .await
    .expect("copy_in_stream");

    let out = exec_stdout(bx, BoxCommand::new("cat").args(["/root/stream-src.txt"])).await;
    assert_eq!(out, "streaming hello");

    let (tar, hint) = bx
        .copy_out_stream("/root/stream-src.txt", CopyOptions::default())
        .await
        .expect("copy_out_stream");
    assert_eq!(
        hint,
        CopySourceKind::File,
        "guest must report a single-file source"
    );

    let dest = tmp.join("stream-dest.txt");
    let report = boxlite_shared::tar::unpack_stream(
        tar,
        dest.clone(),
        boxlite_shared::tar::UnpackContext {
            overwrite: true,
            mkdir_parents: true,
            force_directory: false,
        },
    )
    .await
    .expect("unpack_stream");
    assert_eq!(std::fs::read_to_string(&dest).unwrap(), "streaming hello");
    assert_eq!(
        report.entry_paths,
        vec![std::path::PathBuf::from("stream-src.txt")]
    );
}

/// A single-file tar streamed WITHOUT a shape hint must land as that exact
/// file — the guest's hintless path spools the archive and peeks it to decide
/// (the pre-hint behavior). This is what an old-client upload looks like
/// after the runner relays the body with hint=Unknown.
async fn streaming_hintless_single_file(bx: &LiteBox, tmp: &Path) {
    let host_src = tmp.join("hintless-src.txt");
    std::fs::write(&host_src, b"hintless single\n").unwrap();

    let (_, tar) = boxlite_shared::tar::pack_stream(
        host_src.clone(),
        boxlite_shared::tar::PackContext {
            follow_symlinks: false,
            include_parent: false,
        },
    )
    .await
    .expect("pack_stream");

    bx.copy_in_stream(
        tar,
        "/root/hintless-file.txt",
        CopySourceKind::Unknown,
        CopyOptions::default(),
    )
    .await
    .expect("hintless copy_in_stream");

    let out = exec_stdout(bx, BoxCommand::new("cat").args(["/root/hintless-file.txt"])).await;
    assert_eq!(out, "hintless single\n");
}

/// A directory tree streamed WITHOUT a hint must unpack as a tree. A
/// regression that treated `Unknown` as `File` would force file mode and
/// fail to unpack the directory — the behavior this test pins.
async fn streaming_hintless_directory(bx: &LiteBox, tmp: &Path) {
    let host_dir = tmp.join("hintless-src-dir");
    std::fs::create_dir_all(host_dir.join("sub")).unwrap();
    std::fs::write(host_dir.join("a.txt"), b"hintless dir a\n").unwrap();
    std::fs::write(host_dir.join("sub").join("b.txt"), b"hintless dir b\n").unwrap();

    let (_, tar) = boxlite_shared::tar::pack_stream(
        host_dir.clone(),
        boxlite_shared::tar::PackContext {
            follow_symlinks: false,
            include_parent: true,
        },
    )
    .await
    .expect("pack_stream");

    // Fresh dest, no trailing slash — only the guest's peek can decide.
    bx.copy_in_stream(
        tar,
        "/root/hintless-dir",
        CopySourceKind::Unknown,
        CopyOptions::default(),
    )
    .await
    .expect("hintless directory copy_in_stream");

    let out = exec_stdout(
        bx,
        BoxCommand::new("cat").args(["/root/hintless-dir/hintless-src-dir/a.txt"]),
    )
    .await;
    assert_eq!(out, "hintless dir a\n");
}

/// A tar stream that fails AFTER delivering a complete archive must not
/// surface as a successful copy: the guest extracts the complete prefix and
/// reports success, so the host-side upload has to prefer the source-stream
/// error over the guest's verdict.
async fn streaming_stream_error_after_data_fails(bx: &LiteBox, tmp: &Path) {
    let host_src = tmp.join("stream-err-after-data.txt");
    std::fs::write(&host_src, b"complete archive\n").unwrap();

    let (source_is_dir, mut tar) = boxlite_shared::tar::pack_stream(
        host_src.clone(),
        boxlite_shared::tar::PackContext {
            follow_symlinks: false,
            include_parent: false,
        },
    )
    .await
    .expect("pack_stream");

    let mut items: Vec<std::io::Result<Vec<u8>>> = Vec::new();
    while let Some(item) = tar.next().await {
        items.push(item);
    }
    items.push(Err(std::io::Error::other("injected stream failure")));
    let poisoned: boxlite_shared::BoxByteStream = Box::pin(tokio_stream::iter(items));

    let result = bx
        .copy_in_stream(
            poisoned,
            "/root/stream-err.txt",
            CopySourceKind::from_wire(Some(source_is_dir)),
            CopyOptions::default(),
        )
        .await;
    assert!(
        result.is_err(),
        "a stream error after a complete archive must fail the copy"
    );
}

/// Streaming a directory with recursive=false must be rejected before any
/// data flows, matching the path-based copy_into contract.
async fn streaming_dir_rejects_non_recursive(bx: &LiteBox, tmp: &Path) {
    let host_dir = tmp.join("nonrec-dir");
    std::fs::create_dir_all(host_dir.join("sub")).unwrap();
    std::fs::write(host_dir.join("a.txt"), b"x").unwrap();

    let (source_is_dir, tar) = boxlite_shared::tar::pack_stream(
        host_dir.clone(),
        boxlite_shared::tar::PackContext {
            follow_symlinks: false,
            include_parent: false,
        },
    )
    .await
    .expect("pack_stream");
    assert!(source_is_dir);

    let result = bx
        .copy_in_stream(
            tar,
            "/root/nonrec",
            CopySourceKind::from_wire(Some(source_is_dir)),
            CopyOptions::default().non_recursive(),
        )
        .await;
    assert!(
        result.is_err(),
        "streaming a directory with recursive=false must be rejected"
    );
}

/// A failed hintless upload surfaces as an error through the streaming path.
/// The streamed bytes are not a tar archive, so the guest's peek/extract
/// fails after spooling. (The spool cleanup itself is asserted by the guest
/// crate's `TempUploadGuard` unit tests — the container cannot see the
/// agent's own /tmp.)
async fn streaming_hintless_corrupt_archive_fails(bx: &LiteBox, _tmp: &Path) {
    let tar: boxlite_shared::BoxByteStream =
        Box::pin(tokio_stream::iter(vec![Ok(b"not a tar archive".to_vec())]));

    let result = bx
        .copy_in_stream(
            tar,
            "/root/hintless-fail.txt",
            CopySourceKind::Unknown,
            CopyOptions::default(),
        )
        .await;
    assert!(result.is_err(), "corrupt archive must fail the copy");
}

/// A hinted (streaming) upload whose archive names an entry under a mount
/// must be refused — post-hoc for the stream (it cannot be pre-scanned
/// without spooling) — and the refusal must be *total*: the guest rolls the
/// extracted payload back, so nothing is left half-applied beneath the mount.
async fn streaming_payload_under_mount_is_refused(bx: &LiteBox, tmp: &Path) {
    let host_src = tmp.join("stream-mount-payload.txt");
    std::fs::write(&host_src, "PAYLOAD-STREAM-MOUNT\n").unwrap();

    // The entry lands under the guest's /tmp tmpfs mount: destination root
    // is reachable, only the payload entry is shadowed — exactly the case
    // the staged arm refuses via entry_paths pre-scan. The second entry is
    // reachable and harmless, and is what makes "nothing landed" a claim with
    // something to be false about.
    let mut tar_bytes = Vec::new();
    {
        let mut builder = tar::Builder::new(&mut tar_bytes);
        let content = std::fs::read(&host_src).unwrap();
        for name in ["tmp/stream-mount-payload.txt", "stream-mount-harmless.txt"] {
            let mut header = tar::Header::new_gnu();
            header.set_size(content.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            builder
                .append_data(&mut header, name, &content[..])
                .unwrap();
        }
        builder.finish().unwrap();
    }
    let poisoned: boxlite_shared::BoxByteStream = Box::pin(tokio_stream::iter(vec![Ok(tar_bytes)]));

    let err = bx
        .copy_in_stream(poisoned, "/", CopySourceKind::Dir, CopyOptions::default())
        .await
        .expect_err("a payload under a mount must be refused, not silently shadowed");

    let msg = err.to_string();
    assert!(
        msg.contains("'/tmp' mount"),
        "refusal should name the mount that blocks it, got: {msg}"
    );

    // Same invariant the staged arm gets from refusing before it writes.
    let code = exec_exit_code(
        bx,
        BoxCommand::new("test").args(["-e", "/stream-mount-harmless.txt"]),
    )
    .await;
    assert_eq!(code, 1, "a refused streamed copy must leave nothing behind");
}

#[tokio::test(flavor = "multi_thread")]
async fn copy_integration() {
    let home = boxlite_test_utils::home::PerTestBoxHome::new();
    let runtime = BoxliteRuntime::new(BoxliteOptions {
        home_dir: home.path.clone(),
        image_registries: common::test_registries(),
    })
    .expect("create runtime");
    let bx = runtime
        .create(common::alpine_opts(), None)
        .await
        .expect("create box");
    bx.start().await.expect("start box");

    let tmp = TempDir::new_in("/tmp").unwrap();

    // Run all sub-tests sequentially on the same box.
    // Each uses a unique container path prefix to avoid interference.
    single_file_roundtrip(&bx, tmp.path()).await;
    directory_roundtrip(&bx, tmp.path()).await;
    nested_directory_roundtrip(&bx, tmp.path()).await;
    empty_file_roundtrip(&bx, tmp.path()).await;
    empty_directory_roundtrip(&bx, tmp.path()).await;
    binary_content_fidelity(&bx, tmp.path()).await;
    filename_with_spaces(&bx, tmp.path()).await;
    overwrite_true_replaces_file(&bx, tmp.path()).await;
    overwrite_false_rejects_copy_in(&bx, tmp.path()).await;
    overwrite_false_rejects_copy_in_dir(&bx, tmp.path()).await;
    overwrite_false_rejects_copy_out(&bx, tmp.path()).await;
    non_recursive_rejects_directory(&bx, tmp.path()).await;
    follow_symlinks_false_preserves_link(&bx, tmp.path()).await;
    follow_symlinks_true_dereferences(&bx, tmp.path()).await;
    include_parent_true_nests_dir(&bx, tmp.path()).await;
    include_parent_false_flattens(&bx, tmp.path()).await;
    copy_in_creates_intermediate_dirs(&bx, tmp.path()).await;
    copy_out_nonexistent_errors(&bx, tmp.path()).await;
    concurrent_copy_roundtrip(&bx, tmp.path()).await;
    copy_in_to_tmpfs_is_refused(&bx, tmp.path()).await;
    copy_out_from_tmpfs_is_refused(&bx, tmp.path()).await;
    copy_in_beside_a_file_mount_is_allowed(&bx, tmp.path()).await;
    copy_in_landing_on_a_file_mount_is_refused(&bx, tmp.path()).await;
    copy_out_of_a_dir_containing_a_mount_is_refused(&bx, tmp.path()).await;
    streaming_roundtrip(&bx, tmp.path()).await;
    streaming_single_file_into_existing_dir(&bx, tmp.path()).await;

    streaming_hintless_single_file(&bx, tmp.path()).await;
    streaming_payload_under_mount_is_refused(&bx, tmp.path()).await;
    streaming_hintless_directory(&bx, tmp.path()).await;
    streaming_hintless_corrupt_archive_fails(&bx, tmp.path()).await;
    streaming_stream_error_after_data_fails(&bx, tmp.path()).await;
    streaming_dir_rejects_non_recursive(&bx, tmp.path()).await;

    let _ = runtime.shutdown(Some(common::TEST_SHUTDOWN_TIMEOUT)).await;
}

// ============================================================================
// BASIC ROUND-TRIPS
// ============================================================================

async fn single_file_roundtrip(bx: &LiteBox, tmp: &Path) {
    eprintln!("  [copy] single_file_roundtrip");
    let content = "hello boxlite\n";
    let src = tmp.join("input.txt");
    std::fs::write(&src, content).unwrap();

    bx.copy_into(&src, "/root/input.txt", CopyOptions::default())
        .await
        .expect("copy_into failed");

    let out = exec_stdout(bx, BoxCommand::new("cat").args(["/root/input.txt"])).await;
    assert_eq!(out, content);

    let dst = tmp.join("output.txt");
    bx.copy_out("/root/input.txt", &dst, CopyOptions::default())
        .await
        .expect("copy_out failed");
    assert_eq!(std::fs::read_to_string(&dst).unwrap(), content);
}

async fn directory_roundtrip(bx: &LiteBox, tmp: &Path) {
    eprintln!("  [copy] directory_roundtrip");
    let dir_src = tmp.join("mydir");
    std::fs::create_dir(&dir_src).unwrap();
    std::fs::write(dir_src.join("a.txt"), "aaa\n").unwrap();
    std::fs::write(dir_src.join("b.txt"), "bbb\n").unwrap();

    // Default include_parent=true → creates /root/mydir/{a,b}.txt
    bx.copy_into(&dir_src, "/root", CopyOptions::default())
        .await
        .expect("copy_into dir failed");

    let ls = exec_stdout(bx, BoxCommand::new("ls").args(["/root/mydir"])).await;
    assert!(ls.contains("a.txt"));
    assert!(ls.contains("b.txt"));

    let dir_dst = tmp.join("dir-out");
    std::fs::create_dir(&dir_dst).unwrap();
    bx.copy_out("/root/mydir", &dir_dst, CopyOptions::default())
        .await
        .expect("copy_out dir failed");

    assert_eq!(
        std::fs::read_to_string(dir_dst.join("mydir").join("a.txt")).unwrap(),
        "aaa\n"
    );
    assert_eq!(
        std::fs::read_to_string(dir_dst.join("mydir").join("b.txt")).unwrap(),
        "bbb\n"
    );
}

async fn nested_directory_roundtrip(bx: &LiteBox, tmp: &Path) {
    eprintln!("  [copy] nested_directory_roundtrip");
    let dir_src = tmp.join("deep");
    std::fs::create_dir_all(dir_src.join("a").join("b").join("c")).unwrap();
    std::fs::write(
        dir_src.join("a").join("b").join("c").join("file.txt"),
        "deep\n",
    )
    .unwrap();
    std::fs::write(dir_src.join("top.txt"), "top\n").unwrap();

    bx.copy_into(&dir_src, "/root", CopyOptions::default())
        .await
        .expect("copy_into nested failed");

    let out = exec_stdout(
        bx,
        BoxCommand::new("cat").args(["/root/deep/a/b/c/file.txt"]),
    )
    .await;
    assert_eq!(out, "deep\n");

    let out = exec_stdout(bx, BoxCommand::new("cat").args(["/root/deep/top.txt"])).await;
    assert_eq!(out, "top\n");

    let dir_dst = tmp.join("deep-out");
    std::fs::create_dir(&dir_dst).unwrap();
    bx.copy_out("/root/deep", &dir_dst, CopyOptions::default())
        .await
        .expect("copy_out nested failed");

    assert_eq!(
        std::fs::read_to_string(
            dir_dst
                .join("deep")
                .join("a")
                .join("b")
                .join("c")
                .join("file.txt")
        )
        .unwrap(),
        "deep\n"
    );
}

async fn empty_file_roundtrip(bx: &LiteBox, tmp: &Path) {
    eprintln!("  [copy] empty_file_roundtrip");
    let src = tmp.join("empty.txt");
    std::fs::write(&src, "").unwrap();

    bx.copy_into(&src, "/root/empty.txt", CopyOptions::default())
        .await
        .expect("copy_into empty failed");

    let out = exec_stdout(bx, BoxCommand::new("cat").args(["/root/empty.txt"])).await;
    assert_eq!(out, "");

    let dst = tmp.join("empty-out.txt");
    bx.copy_out("/root/empty.txt", &dst, CopyOptions::default())
        .await
        .expect("copy_out empty failed");
    assert_eq!(std::fs::read(&dst).unwrap().len(), 0);
}

async fn empty_directory_roundtrip(bx: &LiteBox, tmp: &Path) {
    eprintln!("  [copy] empty_directory_roundtrip");
    let dir_src = tmp.join("emptydir");
    std::fs::create_dir(&dir_src).unwrap();

    bx.copy_into(&dir_src, "/root", CopyOptions::default())
        .await
        .expect("copy_into emptydir failed");

    let ls = exec_stdout(bx, BoxCommand::new("ls").args(["/root/emptydir"])).await;
    assert!(ls.trim().is_empty());
}

async fn binary_content_fidelity(bx: &LiteBox, tmp: &Path) {
    eprintln!("  [copy] binary_content_fidelity");
    let data: Vec<u8> = (0..=255).collect();
    let src = tmp.join("binary.bin");
    std::fs::write(&src, &data).unwrap();

    bx.copy_into(&src, "/root/binary.bin", CopyOptions::default())
        .await
        .expect("copy_into binary failed");

    let dst = tmp.join("binary-out.bin");
    bx.copy_out("/root/binary.bin", &dst, CopyOptions::default())
        .await
        .expect("copy_out binary failed");
    assert_eq!(std::fs::read(&dst).unwrap(), data);
}

async fn filename_with_spaces(bx: &LiteBox, tmp: &Path) {
    eprintln!("  [copy] filename_with_spaces");
    let src = tmp.join("my file.txt");
    std::fs::write(&src, "spaces\n").unwrap();

    bx.copy_into(&src, "/root/my file.txt", CopyOptions::default())
        .await
        .expect("copy_into spaces failed");

    let out = exec_stdout(bx, BoxCommand::new("cat").args(["/root/my file.txt"])).await;
    assert_eq!(out, "spaces\n");

    let dst = tmp.join("my file out.txt");
    bx.copy_out("/root/my file.txt", &dst, CopyOptions::default())
        .await
        .expect("copy_out spaces failed");
    assert_eq!(std::fs::read_to_string(&dst).unwrap(), "spaces\n");
}

// ============================================================================
// COPY OPTIONS: overwrite
// ============================================================================

async fn overwrite_true_replaces_file(bx: &LiteBox, tmp: &Path) {
    eprintln!("  [copy] overwrite_true_replaces_file");
    let src = tmp.join("ow.txt");
    std::fs::write(&src, "original\n").unwrap();

    bx.copy_into(&src, "/root/ow.txt", CopyOptions::default())
        .await
        .expect("first copy_into");

    std::fs::write(&src, "updated\n").unwrap();
    bx.copy_into(&src, "/root/ow.txt", CopyOptions::default())
        .await
        .expect("second copy_into with overwrite=true");

    let out = exec_stdout(bx, BoxCommand::new("cat").args(["/root/ow.txt"])).await;
    assert_eq!(out, "updated\n");
}

async fn overwrite_false_rejects_copy_in(bx: &LiteBox, tmp: &Path) {
    eprintln!("  [copy] overwrite_false_rejects_copy_in");
    let src = tmp.join("noo.txt");
    std::fs::write(&src, "first\n").unwrap();

    bx.copy_into(&src, "/root/noo.txt", CopyOptions::default())
        .await
        .expect("first copy_into");

    std::fs::write(&src, "second\n").unwrap();
    let err = bx
        .copy_into(&src, "/root/noo.txt", CopyOptions::default().no_overwrite())
        .await;
    assert!(err.is_err(), "overwrite=false should reject existing file");
}

async fn overwrite_false_rejects_copy_in_dir(bx: &LiteBox, tmp: &Path) {
    eprintln!("  [copy] overwrite_false_rejects_copy_in_dir");
    let dir_src = tmp.join("nodir");
    std::fs::create_dir(&dir_src).unwrap();
    std::fs::write(dir_src.join("x.txt"), "x\n").unwrap();

    bx.copy_into(&dir_src, "/root", CopyOptions::default())
        .await
        .expect("first copy_into dir");

    let err = bx
        .copy_into(&dir_src, "/root", CopyOptions::default().no_overwrite())
        .await;
    assert!(
        err.is_err(),
        "overwrite=false should reject existing directory"
    );
}

async fn overwrite_false_rejects_copy_out(bx: &LiteBox, tmp: &Path) {
    eprintln!("  [copy] overwrite_false_rejects_copy_out");
    let src = tmp.join("out-ow.txt");
    std::fs::write(&src, "data\n").unwrap();
    bx.copy_into(&src, "/root/out-ow.txt", CopyOptions::default())
        .await
        .expect("copy_into");

    let dst = tmp.join("existing.txt");
    std::fs::write(&dst, "existing\n").unwrap();

    let err = bx
        .copy_out(
            "/root/out-ow.txt",
            &dst,
            CopyOptions::default().no_overwrite(),
        )
        .await;
    assert!(
        err.is_err(),
        "overwrite=false should reject existing host file"
    );
}

// ============================================================================
// COPY OPTIONS: recursive
// ============================================================================

async fn non_recursive_rejects_directory(bx: &LiteBox, tmp: &Path) {
    eprintln!("  [copy] non_recursive_rejects_directory");
    let dir_src = tmp.join("norecurse");
    std::fs::create_dir(&dir_src).unwrap();
    std::fs::write(dir_src.join("f.txt"), "f\n").unwrap();

    let err = bx
        .copy_into(
            &dir_src,
            "/root/norecurse",
            CopyOptions::default().non_recursive(),
        )
        .await;
    assert!(
        err.is_err(),
        "non_recursive should reject directory copy_into"
    );
}

// ============================================================================
// COPY OPTIONS: follow_symlinks
// ============================================================================

async fn follow_symlinks_false_preserves_link(bx: &LiteBox, tmp: &Path) {
    eprintln!("  [copy] follow_symlinks_false_preserves_link");
    let dir_src = tmp.join("linkdir");
    std::fs::create_dir(&dir_src).unwrap();
    std::fs::write(dir_src.join("target.txt"), "target content\n").unwrap();
    std::os::unix::fs::symlink("target.txt", dir_src.join("link.txt")).unwrap();

    bx.copy_into(
        &dir_src,
        "/root",
        CopyOptions::default().follow_symlinks(false),
    )
    .await
    .expect("copy_into with symlink");

    let out = exec_stdout(
        bx,
        BoxCommand::new("readlink").args(["/root/linkdir/link.txt"]),
    )
    .await;
    assert_eq!(out.trim(), "target.txt");

    let out = exec_stdout(bx, BoxCommand::new("cat").args(["/root/linkdir/link.txt"])).await;
    assert_eq!(out, "target content\n");
}

async fn follow_symlinks_true_dereferences(bx: &LiteBox, tmp: &Path) {
    eprintln!("  [copy] follow_symlinks_true_dereferences");
    let dir_src = tmp.join("derefdir");
    std::fs::create_dir(&dir_src).unwrap();
    std::fs::write(dir_src.join("target.txt"), "deref content\n").unwrap();
    std::os::unix::fs::symlink("target.txt", dir_src.join("link.txt")).unwrap();

    bx.copy_into(
        &dir_src,
        "/root",
        CopyOptions::default().follow_symlinks(true),
    )
    .await
    .expect("copy_into with follow_symlinks");

    let exit = exec_exit_code(
        bx,
        BoxCommand::new("readlink").args(["/root/derefdir/link.txt"]),
    )
    .await;
    assert_ne!(exit, 0, "readlink should fail on dereferenced file");

    let out = exec_stdout(bx, BoxCommand::new("cat").args(["/root/derefdir/link.txt"])).await;
    assert_eq!(out, "deref content\n");
}

// ============================================================================
// COPY OPTIONS: include_parent
// ============================================================================

async fn include_parent_true_nests_dir(bx: &LiteBox, tmp: &Path) {
    eprintln!("  [copy] include_parent_true_nests_dir");
    let dir_src = tmp.join("parentdir");
    std::fs::create_dir(&dir_src).unwrap();
    std::fs::write(dir_src.join("p.txt"), "parent\n").unwrap();

    bx.copy_into(
        &dir_src,
        "/root",
        CopyOptions::default().include_parent(true),
    )
    .await
    .expect("copy_into include_parent=true");

    let out = exec_stdout(bx, BoxCommand::new("cat").args(["/root/parentdir/p.txt"])).await;
    assert_eq!(out, "parent\n");
}

async fn include_parent_false_flattens(bx: &LiteBox, tmp: &Path) {
    eprintln!("  [copy] include_parent_false_flattens");
    let dir_src = tmp.join("flatdir");
    std::fs::create_dir(&dir_src).unwrap();
    std::fs::write(dir_src.join("f.txt"), "flat\n").unwrap();

    bx.copy_into(
        &dir_src,
        "/root/flatdest/",
        CopyOptions::default().include_parent(false),
    )
    .await
    .expect("copy_into include_parent=false");

    let out = exec_stdout(bx, BoxCommand::new("cat").args(["/root/flatdest/f.txt"])).await;
    assert_eq!(out, "flat\n");
}

// ============================================================================
// ERROR / EDGE CASES
// ============================================================================

async fn copy_in_creates_intermediate_dirs(bx: &LiteBox, tmp: &Path) {
    eprintln!("  [copy] copy_in_creates_intermediate_dirs");
    let src = tmp.join("mkdirs.txt");
    std::fs::write(&src, "nested\n").unwrap();

    bx.copy_into(
        &src,
        "/root/deep/new/path/mkdirs.txt",
        CopyOptions::default(),
    )
    .await
    .expect("copy_into with intermediate dirs");

    let out = exec_stdout(
        bx,
        BoxCommand::new("cat").args(["/root/deep/new/path/mkdirs.txt"]),
    )
    .await;
    assert_eq!(out, "nested\n");
}

async fn copy_out_nonexistent_errors(bx: &LiteBox, tmp: &Path) {
    eprintln!("  [copy] copy_out_nonexistent_errors");
    let dst = tmp.join("nope.txt");
    let err = bx
        .copy_out("/root/does-not-exist-xyz", &dst, CopyOptions::default())
        .await;
    assert!(err.is_err(), "copy_out nonexistent should error");
}

// ============================================================================
// CONCURRENCY
// ============================================================================

async fn concurrent_copy_roundtrip(bx: &LiteBox, tmp: &Path) {
    eprintln!("  [copy] concurrent_copy_roundtrip");

    let futs: Vec<_> = (0..5u32)
        .map(|i| async move {
            let content = format!("concurrent-{}\n", i);
            let src = tmp.join(format!("conc-in-{}.txt", i));
            std::fs::write(&src, &content).unwrap();

            let container_path = format!("/root/conc-{}/file.txt", i);
            bx.copy_into(&src, &container_path, CopyOptions::default())
                .await
                .unwrap_or_else(|e| panic!("copy_into {} failed: {}", i, e));

            let dst = tmp.join(format!("conc-out-{}.txt", i));
            bx.copy_out(&container_path, &dst, CopyOptions::default())
                .await
                .unwrap_or_else(|e| panic!("copy_out {} failed: {}", i, e));

            let got = std::fs::read_to_string(&dst).unwrap();
            assert_eq!(got, content, "roundtrip mismatch for task {}", i);
        })
        .collect();

    futures::future::join_all(futs).await;
}

// ============================================================================
// MOUNT SHADOWING (POL-305)
//
// `/tmp` is a tmpfs declared in the container's OCI spec and mounted inside
// the container's own mount namespace. A copy that resolves the destination
// against the guest-side rootfs writes *under* that tmpfs, where no process in
// the box can ever see it — and reads back its own shadow rather than what the
// workload actually wrote.
// ============================================================================

async fn copy_in_to_tmpfs_is_refused(bx: &LiteBox, tmp: &Path) {
    eprintln!("  [copy] copy_in_to_tmpfs_is_refused");
    let src = tmp.join("tmpfs-in.txt");
    std::fs::write(&src, "PAYLOAD-IN-1234\n").unwrap();

    // Pre-fix this "succeeds" and the bytes land on the rootfs layer beneath
    // the tmpfs, where nothing in the box can ever see them.
    let err = bx
        .copy_into(&src, "/tmp/tmpfs-in.txt", CopyOptions::default())
        .await
        .expect_err("copy_into a tmpfs path must be refused, not silently shadowed");

    // The exact fragment, not a bare "/tmp": examples/python/02_features/
    // copy_files.py and examples/node/cp_tmpfs_workaround.js both fail closed on
    // it, and the runtime's own staging tar path would satisfy a looser match.
    let msg = err.to_string();
    assert!(
        msg.contains("'/tmp' mount"),
        "refusal should name the mount that blocks it, got: {msg}"
    );

    // And nothing must have been written to the shadow on the way to failing.
    let code = exec_exit_code(
        bx,
        BoxCommand::new("test").args(["-e", "/tmp/tmpfs-in.txt"]),
    )
    .await;
    assert_eq!(code, 1, "refused copy must leave no file behind");
}

async fn copy_out_from_tmpfs_is_refused(bx: &LiteBox, tmp: &Path) {
    eprintln!("  [copy] copy_out_from_tmpfs_is_refused");

    // The workload writes into its own tmpfs.
    let written = exec_stdout(
        bx,
        BoxCommand::new("sh").args([
            "-c",
            "printf 'PAYLOAD-OUT-5678\\n' > /tmp/tmpfs-out.txt && cat /tmp/tmpfs-out.txt",
        ]),
    )
    .await;
    assert_eq!(written, "PAYLOAD-OUT-5678\n", "workload sees its own write");

    // Pre-fix this reports "source path does not exist" for a file that plainly
    // does exist — or worse, hands back a stale rootfs artifact.
    let dst = tmp.join("tmpfs-out.txt");
    let err = bx
        .copy_out("/tmp/tmpfs-out.txt", &dst, CopyOptions::default())
        .await
        .expect_err("copy_out of a tmpfs path must be refused, not answered from the shadow");

    let msg = err.to_string();
    assert!(
        msg.contains("'/tmp' mount"),
        "refusal should name the mount that blocks it, got: {msg}"
    );
    assert!(!dst.exists(), "refused copy_out must not write a host file");
}

/// `/etc` is not itself a mount — only `/etc/{hostname,hosts,resolv.conf}` are
/// binds under it. Writing *into* `/etc` is therefore fine as long as no entry
/// lands on one of those binds.
///
/// Both destination shapes are exercised on purpose. The directory form is the
/// one that goes through the per-entry check: a refusal written as "the
/// destination's subtree contains a mount" would pass the file case and still
/// break the shipped `copy_in(motd, "/etc")` example.
async fn copy_in_beside_a_file_mount_is_allowed(bx: &LiteBox, tmp: &Path) {
    eprintln!("  [copy] copy_in_beside_a_file_mount_is_allowed");

    // File destination.
    let src = tmp.join("motd.txt");
    std::fs::write(&src, "Welcome to BoxLite!\n").unwrap();
    bx.copy_into(&src, "/etc/motd.txt", CopyOptions::default())
        .await
        .expect("copy_in to a file beside a mount must be allowed");
    let out = exec_stdout(bx, BoxCommand::new("cat").args(["/etc/motd.txt"])).await;
    assert_eq!(out, "Welcome to BoxLite!\n");

    // Directory destination — `/etc` holds three binds, but nothing in this
    // payload lands on one, so the copy must go through.
    let dir = tmp.join("etcdrop");
    std::fs::create_dir(&dir).unwrap();
    std::fs::write(dir.join("issue.net"), "boxlite\n").unwrap();
    bx.copy_into(&dir, "/etc/", CopyOptions::default())
        .await
        .expect("copy_in into a dir that merely contains mounts must be allowed");
    let out = exec_stdout(bx, BoxCommand::new("cat").args(["/etc/etcdrop/issue.net"])).await;
    assert_eq!(out, "boxlite\n");
}

/// The same directory destination, but now the payload lands *on* a bind. The
/// root is reachable; the entry is not — so the per-entry check must catch it.
async fn copy_in_landing_on_a_file_mount_is_refused(bx: &LiteBox, tmp: &Path) {
    eprintln!("  [copy] copy_in_landing_on_a_file_mount_is_refused");
    let dir = tmp.join("etcclash");
    std::fs::create_dir(&dir).unwrap();
    std::fs::write(dir.join("hosts"), "127.0.0.1 evil\n").unwrap();
    std::fs::write(dir.join("harmless.txt"), "ok\n").unwrap();

    // include_parent=false flattens the contents into /etc, so `hosts` lands
    // squarely on the bind — the whole point of the case.
    let err = bx
        .copy_into(&dir, "/etc/", CopyOptions::default().include_parent(false))
        .await
        .expect_err("an entry landing on a bind mount must be refused");

    // Same fragment the tmpfs cases pin: a caller taught to match `'<mount>'
    // mount` meets this wording too, and `/etc/hosts` alone would still match a
    // message that had stopped naming the mount as a mount.
    let msg = err.to_string();
    assert!(
        msg.contains("'/etc/hosts' mount"),
        "refusal should name the mount the entry would land on, got: {msg}"
    );

    // The refusal must be total — the harmless sibling must not have landed
    // either, or a partially applied copy is left behind.
    let code = exec_exit_code(
        bx,
        BoxCommand::new("test").args(["-e", "/etc/harmless.txt"]),
    )
    .await;
    assert_eq!(code, 1, "refused copy must leave nothing behind");
}

/// Reading a directory that *contains* a mount walks the rootfs layer, so the
/// archive would carry the image's `/etc/hosts` rather than the bind the box
/// actually has — the same shadow, one level down.
async fn copy_out_of_a_dir_containing_a_mount_is_refused(bx: &LiteBox, tmp: &Path) {
    eprintln!("  [copy] copy_out_of_a_dir_containing_a_mount_is_refused");
    let dst = tmp.join("etc-copy");
    let err = bx
        .copy_out("/etc", &dst, CopyOptions::default())
        .await
        .expect_err("copy_out of a directory containing a mount must be refused");

    // Which of the three `/etc` binds is named depends on their order in the
    // OCI spec, so accept any — but insist on the quoted `'<mount>' mount`
    // fragment. A bare `/etc/` also appears in the remedy text, so it would
    // hold even if the message stopped naming a mount at all.
    let msg = err.to_string();
    assert!(
        [
            "'/etc/hosts' mount",
            "'/etc/hostname' mount",
            "'/etc/resolv.conf' mount"
        ]
        .iter()
        .any(|fragment| msg.contains(fragment)),
        "refusal should name the mount inside it, got: {msg}"
    );
}
