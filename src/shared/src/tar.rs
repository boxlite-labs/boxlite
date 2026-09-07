//! Tar archive pack/unpack for host↔guest file transfer.
//!
//! Both host (boxlite) and guest agent share this module to avoid
//! duplicating tar building/extraction logic.

use crate::constants::files::{COPY_CHUNKS_IN_FLIGHT, COPY_CHUNK_SIZE};
use crate::{BoxByteStream, BoxliteError, BoxliteResult};
use futures::StreamExt;
use std::collections::HashSet;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

// ── Pack ──────────────────────────────────────────────────────────

/// Controls how a source path is packed into a tar archive.
pub struct PackContext {
    /// Follow symlinks (copy target content) vs preserve them as links.
    pub follow_symlinks: bool,
    /// When packing a directory, include the directory itself as a top-level
    /// entry (true) or flatten its contents into the archive root (false).
    pub include_parent: bool,
}

/// Pack `src` into `writer` (generic over `std::io::Write`).
fn pack_blocking<W: Write>(src: &Path, writer: W, opts: &PackContext) -> BoxliteResult<()> {
    let mut builder = tar::Builder::new(writer);
    builder.follow_symlinks(opts.follow_symlinks);

    if src.is_dir() {
        if opts.include_parent {
            let base = src
                .file_name()
                .map(|s| s.to_owned())
                .unwrap_or_else(|| std::ffi::OsStr::new("root").to_owned());
            builder
                .append_dir_all(base, src)
                .map_err(|e| BoxliteError::Storage(format!("failed to archive dir: {}", e)))?;
        } else {
            // Add each top-level entry individually so we don't create a
            // "." entry that produces an empty tar path on extraction.
            for entry in std::fs::read_dir(src).map_err(|e| {
                BoxliteError::Storage(format!("failed to read dir {}: {}", src.display(), e))
            })? {
                let entry = entry.map_err(|e| {
                    BoxliteError::Storage(format!("failed to read dir entry: {}", e))
                })?;
                let name = entry.file_name();
                let path = entry.path();
                if path.is_dir() {
                    builder.append_dir_all(&name, &path).map_err(|e| {
                        BoxliteError::Storage(format!("failed to archive dir: {}", e))
                    })?;
                } else {
                    builder.append_path_with_name(&path, &name).map_err(|e| {
                        BoxliteError::Storage(format!("failed to archive file: {}", e))
                    })?;
                }
            }
        }
    } else {
        let name = src
            .file_name()
            .ok_or_else(|| BoxliteError::Config("source file has no name".into()))?;
        builder
            .append_path_with_name(src, name)
            .map_err(|e| BoxliteError::Storage(format!("failed to archive file: {}", e)))?;
    }

    builder
        .finish()
        .map_err(|e| BoxliteError::Storage(format!("failed to finish tar: {}", e)))?;

    // `finish` writes the trailer but not the writer's own pending buffer
    // (PipeWriter coalesces the final sub-chunk-size bytes). Flush it here
    // so a dropped consumer surfaces as an error instead of silently
    // truncating the stream.
    let mut inner = builder
        .into_inner()
        .map_err(|e| BoxliteError::Storage(format!("failed to finish tar: {}", e)))?;
    inner
        .flush()
        .map_err(|e| BoxliteError::Storage(format!("failed to flush tar stream: {}", e)))
}

// ── Stream pack ──────────────────────────────────────────────────

/// Blocking `Write` adapter feeding a bounded channel. Coalesces small writes
/// into full `COPY_CHUNK_SIZE` messages; `blocking_send` provides backpressure
/// (a slow consumer throttles the pack thread) and surfaces `BrokenPipe` when
/// the consumer drops the stream.
struct PipeWriter {
    tx: mpsc::Sender<io::Result<Vec<u8>>>,
    pending: Vec<u8>,
}

impl PipeWriter {
    fn send_chunk(&self, chunk: Vec<u8>) -> io::Result<()> {
        self.tx
            .blocking_send(Ok(chunk))
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "tar stream consumer dropped"))
    }

    fn flush_pending(&mut self) -> io::Result<()> {
        if self.pending.is_empty() {
            return Ok(());
        }
        let chunk = std::mem::take(&mut self.pending);
        self.send_chunk(chunk)
    }
}

impl Write for PipeWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }

        let write_len = (COPY_CHUNK_SIZE - self.pending.len()).min(buf.len());
        self.pending.extend_from_slice(&buf[..write_len]);
        if self.pending.len() == COPY_CHUNK_SIZE {
            let chunk = std::mem::take(&mut self.pending);
            self.send_chunk(chunk)?;
            self.pending = Vec::with_capacity(COPY_CHUNK_SIZE);
        }
        Ok(write_len)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.flush_pending()
    }
}

impl Drop for PipeWriter {
    fn drop(&mut self) {
        let _ = self.flush_pending();
    }
}

/// The concrete stream [`pack_stream`] produces.
///
/// Deliberately *not* erased to [`BoxByteStream`]. Handing a `dyn` stream to
/// the generic gRPC upload defeats rustc's auto-trait leak check inside an
/// `#[async_trait]` method — the whole transfer then fails to compile with
/// "`Send` is not general enough". Callers that genuinely need erasure box it
/// themselves; the ones that just forward it keep the concrete type.
pub type PackedByteStream = ReceiverStream<io::Result<Vec<u8>>>;

/// Stream `src` into a tar byte stream.
///
/// Returns the source shape (`source_is_dir = src.is_dir()`) alongside the
/// stream. The pack runs on a `spawn_blocking` thread; a pack failure surfaces
/// as a terminal `Err` item on the stream.
pub async fn pack_stream(
    src: PathBuf,
    opts: PackContext,
) -> BoxliteResult<(bool, PackedByteStream)> {
    let source_is_dir = src.is_dir();
    if !src.exists() {
        return Err(BoxliteError::NotFound(format!(
            "source path {} does not exist",
            src.display()
        )));
    }
    let (tx, rx) = mpsc::channel::<io::Result<Vec<u8>>>(COPY_CHUNKS_IN_FLIGHT);
    let writer = PipeWriter {
        tx: tx.clone(),
        pending: Vec::with_capacity(COPY_CHUNK_SIZE),
    };
    let task_tx = tx;
    tokio::task::spawn_blocking(move || {
        let outcome = pack_task_body(&src, writer, &opts);
        // Report a failure as a terminal stream error through the original
        // sender, then drop it to signal EOF.
        if let Err(e) = outcome {
            let _ = task_tx.blocking_send(Err(io::Error::other(e)));
        }
    });
    Ok((source_is_dir, ReceiverStream::new(rx)))
}

/// Body of the pack task: runs [`pack_blocking`] under `catch_unwind` so a
/// panicking pack surfaces as an error (per the stream contract) instead of
/// ending the stream on a clean EOF that consumers would read as a truncated
/// archive.
fn pack_task_body<W: Write>(src: &Path, writer: W, opts: &PackContext) -> BoxliteResult<()> {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        pack_blocking(src, writer, opts)
    })) {
        Ok(outcome) => outcome,
        Err(_) => Err(BoxliteError::Internal("tar pack task panicked".into())),
    }
}

// ── Unpack ────────────────────────────────────────────────────────

/// An error's own message followed by every cause beneath it.
///
/// `tar`'s `Display` names the operation it was attempting and drops the errno
/// that stopped it, so `failed to create \`/x/y\`` reaches the caller with no
/// hint whether the disk was full, the path was not a directory, or permission
/// was denied. Walking `source()` puts the reason back.
fn with_causes(err: &dyn std::error::Error) -> String {
    let mut detail = err.to_string();
    let mut cause = err.source();
    while let Some(next) = cause {
        detail.push_str(&format!(": {}", next));
        cause = next.source();
    }
    detail
}

/// Controls how a tar archive is unpacked to a destination.
pub struct UnpackContext {
    /// Allow overwriting existing files/directories.
    pub overwrite: bool,
    /// Create parent directories if they don't exist.
    pub mkdir_parents: bool,
    /// Force directory extraction mode (skip single-file detection).
    /// Set `true` when the caller knows the destination is a directory
    /// (e.g. original path had trailing `/`).
    pub force_directory: bool,
}

/// Paths observed while unpacking a one-shot tar stream.
#[derive(Debug, Default)]
pub struct UnpackReport {
    /// Extracted paths relative to the destination, including implied directories.
    pub entry_paths: Vec<PathBuf>,
    /// Entry directories that existed before streamed extraction reached them.
    ///
    /// These paths are relative to the destination and are only collected when
    /// extracting into a directory.
    pub preexisting_dirs: Vec<PathBuf>,
}

/// Unpack a tar archive to `dest`.
///
/// Automatically detects whether to extract as a single file (FileToFile)
/// or into a directory (IntoDirectory) based on tar contents and dest path,
/// unless `force_directory` is set.
///
/// Runs blocking I/O on a dedicated thread via `spawn_blocking`.
pub async fn unpack(tar_path: PathBuf, dest: PathBuf, opts: UnpackContext) -> BoxliteResult<()> {
    tokio::task::spawn_blocking(move || unpack_blocking(&tar_path, &dest, &opts))
        .await
        .map_err(|e| BoxliteError::Storage(format!("unpack task join error: {}", e)))?
}

fn unpack_blocking(tar_path: &Path, dest: &Path, opts: &UnpackContext) -> BoxliteResult<()> {
    let mode = if opts.force_directory {
        ExtractionMode::IntoDirectory
    } else {
        detect_extraction_mode(dest, tar_path)?
    };
    let tar_file = std::fs::File::open(tar_path).map_err(|e| {
        BoxliteError::Storage(format!("failed to open tar {}: {}", tar_path.display(), e))
    })?;
    extract_from_reader(tar_file, dest, opts, mode, None)
}

/// Extract a tar archive read from `reader` to `dest` using the given `mode`.
fn extract_from_reader<R: Read>(
    reader: R,
    dest: &Path,
    opts: &UnpackContext,
    mode: ExtractionMode,
    mut report: Option<&mut UnpackReport>,
) -> BoxliteResult<()> {
    let mut seen = HashSet::new();
    let mut record = |path: &Path, existing_root: Option<&Path>| {
        if let Some(report) = report.as_deref_mut() {
            record_paths(report, &mut seen, existing_root, path);
        }
    };
    match mode {
        ExtractionMode::FileToFile => {
            if let Some(parent) = dest.parent() {
                if opts.mkdir_parents && !parent.exists() {
                    std::fs::create_dir_all(parent).map_err(|e| {
                        BoxliteError::Storage(format!(
                            "failed to create parent dir {}: {}",
                            parent.display(),
                            e
                        ))
                    })?;
                } else if !parent.exists() {
                    return Err(BoxliteError::Storage(format!(
                        "parent directory of {} does not exist",
                        dest.display()
                    )));
                }
            }
            if !opts.overwrite && dest.exists() {
                return Err(BoxliteError::Storage(format!(
                    "destination {} exists and overwrite=false",
                    dest.display()
                )));
            }
            let mut archive = tar::Archive::new(reader);
            let mut entries = archive
                .entries()
                .map_err(|e| BoxliteError::Storage(format!("failed to read tar entries: {}", e)))?;
            if let Some(entry) = entries.next() {
                let mut entry = entry.map_err(|e| {
                    BoxliteError::Storage(format!("failed to read tar entry: {}", e))
                })?;
                if let Ok(path) = entry.path() {
                    record(path.as_ref(), None);
                }
                entry.unpack(dest).map_err(|e| {
                    BoxliteError::Storage(format!(
                        "failed to unpack file to {}: {}",
                        dest.display(),
                        with_causes(&e)
                    ))
                })?;
            }
            Ok(())
        }
        ExtractionMode::IntoDirectory => {
            if !dest.exists() {
                if opts.mkdir_parents {
                    std::fs::create_dir_all(dest).map_err(|e| {
                        BoxliteError::Storage(format!(
                            "failed to create destination {}: {}",
                            dest.display(),
                            e
                        ))
                    })?;
                } else {
                    return Err(BoxliteError::Storage(format!(
                        "destination {} does not exist",
                        dest.display()
                    )));
                }
            }
            if dest.exists() && !opts.overwrite {
                return Err(BoxliteError::Storage(format!(
                    "destination {} exists and overwrite=false",
                    dest.display()
                )));
            }
            let mut archive = tar::Archive::new(reader);
            // The same pass tar::Archive::unpack would run, with one
            // addition: every extracted name is recorded (sanitized, with
            // implied directories) so callers can hand the created paths to
            // the box user and refuse entries the mounts shadow — without
            // consuming the one-shot stream twice. Directory entries are
            // delayed and sorted the way tar-rs does it (permissions).
            let mut directories = Vec::new();
            for entry in archive
                .entries()
                .map_err(|e| BoxliteError::Storage(format!("failed to read tar entries: {}", e)))?
            {
                let mut file = entry.map_err(|e| {
                    BoxliteError::Storage(format!("failed to read tar entry: {}", e))
                })?;
                if let Ok(path) = file.path() {
                    record(path.as_ref(), Some(dest));
                }
                if file.header().entry_type() == tar::EntryType::Directory {
                    directories.push(file);
                } else {
                    file.unpack_in(dest).map_err(|e| {
                        BoxliteError::Storage(format!(
                            "failed to extract archive: {}",
                            with_causes(&e)
                        ))
                    })?;
                }
            }
            directories.sort_by(|a, b| b.path_bytes().cmp(&a.path_bytes()));
            for mut dir in directories {
                dir.unpack_in(dest).map_err(|e| {
                    BoxliteError::Storage(format!("failed to extract archive: {}", with_causes(&e)))
                })?;
            }
            Ok(())
        }
    }
}

/// Record `path` — sanitized, plus every directory its name implies,
/// outermost first — in `report`, deduplicated through `seen`. Mirrors
/// [`entry_paths_blocking`] so the streamed extraction reports exactly the
/// same path shape the staged path's pre-scan does. When `existing_root` is
/// present, directories that predate their first archive occurrence are also
/// recorded before extraction can create or replace them.
fn record_paths(
    report: &mut UnpackReport,
    seen: &mut HashSet<PathBuf>,
    existing_root: Option<&Path>,
    path: &Path,
) {
    let Some(sanitized) = sanitize_entry_path(path) else {
        return;
    };
    for step in implied_dirs_then_self(&sanitized) {
        if seen.insert(step.clone()) {
            if let Some(root) = existing_root {
                if root.join(&step).is_dir() {
                    report.preexisting_dirs.push(step.clone());
                }
            }
            report.entry_paths.push(step);
        }
    }
}

// ── Entry names ───────────────────────────────────────────────────

/// Paths extraction will create, relative to the extraction root.
///
/// Everything the archive names, plus every directory those names imply.
/// A tar need not carry an entry for a directory it puts files in, and plenty
/// of writers emit only leaves — `PUT /boxes/{id}/files` takes whatever
/// `application/x-tar` the caller built. Extraction conjures the missing
/// directories regardless (`tar-0.4.45/src/entry.rs`, `ensure_dir_created`),
/// so stopping at the leaves under-reports what the copy made: the guest hands
/// the box user only what this list names, and an unnamed directory stays
/// root-owned around a file the workload does own.
///
/// The tar is caller-supplied, so its entry names are untrusted: an entry may
/// be absolute (`/etc/shadow`) or climb out (`../../x`). Extraction already
/// refuses both, so this applies the same rule — otherwise a later
/// `dest.join(rel)` would escape the destination entirely, since `Path::join`
/// discards the base when handed an absolute path.
///
/// Header-only in intent, but not in cost: `tar::Archive::entries()` leaves
/// the reader's `Seek` impl unused, so stepping over an entry's payload reads
/// it (`tar-0.4.45/src/archive.rs`, `fn skip`). Walking an archive costs the
/// whole archive, which is why this runs on a blocking thread like [`pack`]
/// and [`unpack`].
///
/// A malformed archive yields the names read so far rather than an error:
/// extraction reads the same bytes moments later and reports the real parse
/// error, and until then the worst case is that no names are found.
pub async fn entry_paths(tar_path: PathBuf) -> BoxliteResult<Vec<PathBuf>> {
    tokio::task::spawn_blocking(move || entry_paths_blocking(&tar_path))
        .await
        .map_err(|e| BoxliteError::Storage(format!("entry_paths task join error: {}", e)))
}

fn entry_paths_blocking(tar_path: &Path) -> Vec<PathBuf> {
    let Ok(file) = std::fs::File::open(tar_path) else {
        return Vec::new();
    };
    let mut archive = tar::Archive::new(file);
    let Ok(entries) = archive.entries() else {
        return Vec::new();
    };

    // Deduplicated because a directory is usually implied by many entries —
    // and often named outright as well, once the archive carries it and again
    // through each file inside it.
    let mut created = Vec::new();
    let mut seen = HashSet::new();
    for path in entries
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| entry.path().ok().map(|path| path.into_owned()))
        .filter_map(|path| sanitize_entry_path(&path))
    {
        for step in implied_dirs_then_self(&path) {
            if seen.insert(step.clone()) {
                created.push(step);
            }
        }
    }
    created
}

/// `path` preceded by every directory its own name implies, outermost first.
///
/// `a/b/c.txt` yields `a`, `a/b`, `a/b/c.txt`. Outermost first so a caller
/// walking the list meets a directory before whatever sits inside it.
fn implied_dirs_then_self(path: &Path) -> Vec<PathBuf> {
    let mut chain: Vec<PathBuf> = path
        .ancestors()
        .filter(|ancestor| !ancestor.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .collect();
    chain.reverse();
    chain
}

/// An archive entry name reduced to what extraction would actually create.
///
/// Mirrors tar-rs's own rule (`entry.rs`: `RootDir => continue`,
/// `ParentDir => return Ok(false)`): drop the leading `/`, refuse anything
/// containing `..`. `None` means extraction skipped it, so nothing downstream
/// should touch it either.
fn sanitize_entry_path(path: &Path) -> Option<PathBuf> {
    use std::path::Component;
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => out.push(part),
            // Leading `/` and `.` are dropped by extraction, not rejected.
            Component::RootDir | Component::CurDir => continue,
            Component::ParentDir | Component::Prefix(_) => return None,
        }
    }
    (!out.as_os_str().is_empty()).then_some(out)
}

// ── Stream unpack ────────────────────────────────────────────────

/// Blocking `Read` adapter draining a bounded channel, used to feed a
/// `tar::Archive` from an async byte stream.
struct PipeReader {
    rx: mpsc::Receiver<io::Result<Vec<u8>>>,
    pending: Option<(Vec<u8>, usize)>,
}

impl Read for PipeReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            if let Some((data, pos)) = &mut self.pending {
                let remaining = &data[*pos..];
                let n = remaining.len().min(buf.len());
                buf[..n].copy_from_slice(&remaining[..n]);
                *pos += n;
                if *pos == data.len() {
                    self.pending = None;
                }
                return Ok(n);
            }
            match self.rx.blocking_recv() {
                Some(Ok(data)) => {
                    if data.is_empty() {
                        continue;
                    }
                    self.pending = Some((data, 0));
                }
                Some(Err(e)) => return Err(e),
                None => return Ok(0),
            }
        }
    }
}

/// Unpack a tar byte stream to `dest`.
///
/// Unlike the file-based [`unpack`], there is no archive peek — the extraction
/// shape is taken authoritatively from `opts.force_directory` (`true` →
/// directory, `false` → single file). Callers must resolve that from the peer's
/// `source_is_dir` hint; a missing hint must fall back to the file-based
/// [`unpack`] path, never call here with a guessed bool.
///
/// Returns the extracted paths relative to `dest` (sanitized, implied
/// directories included) and which of those directories already existed, so
/// the caller can hand only newly created paths to the box user and refuse
/// entries the mounts shadow.
pub async fn unpack_stream(
    mut stream: BoxByteStream,
    dest: PathBuf,
    opts: UnpackContext,
) -> BoxliteResult<UnpackReport> {
    let (tx, rx) = mpsc::channel::<io::Result<Vec<u8>>>(COPY_CHUNKS_IN_FLIGHT);
    let forward = tokio::spawn(async move {
        while let Some(item) = stream.next().await {
            if tx.send(item).await.is_err() {
                break;
            }
        }
    });
    let mut reader = PipeReader { rx, pending: None };
    let unpack = tokio::task::spawn_blocking(move || {
        let mode = if opts.force_directory {
            ExtractionMode::IntoDirectory
        } else {
            ExtractionMode::FileToFile
        };
        let mut report = UnpackReport::default();
        extract_from_reader(&mut reader, &dest, &opts, mode, Some(&mut report))?;
        // Extraction can finish before the byte stream ends: FileToFile stops
        // after one entry, and tar-rs stops at the archive end marker. Drain
        // the raw stream so a terminal producer error cannot become success.
        io::copy(&mut reader, &mut io::sink())
            .map_err(|e| BoxliteError::Storage(format!("failed to drain tar stream: {}", e)))?;
        Ok(report)
    });
    let result = unpack.await;
    // Abort the forwarder before mapping the join result, so a panicked
    // unpack task can't leak a forwarder that keeps draining the stream.
    forward.abort();
    result.map_err(|e| BoxliteError::Storage(format!("unpack task join error: {}", e)))?
}

// ── Private ───────────────────────────────────────────────────────

enum ExtractionMode {
    FileToFile,
    IntoDirectory,
}

/// Inspect the destination path and tar contents to decide extraction mode.
///
/// Rules (evaluated in order):
/// 1. Dest path has trailing `/` → directory mode
/// 2. Dest exists as a directory → directory mode
/// 3. Tar contains exactly one regular file → file-to-file mode
/// 4. Fallback → directory mode
fn detect_extraction_mode(dest: &Path, tar_path: &Path) -> BoxliteResult<ExtractionMode> {
    if dest.as_os_str().to_string_lossy().ends_with('/') {
        return Ok(ExtractionMode::IntoDirectory);
    }
    if dest.is_dir() {
        return Ok(ExtractionMode::IntoDirectory);
    }
    let tar_file = std::fs::File::open(tar_path).map_err(|e| {
        BoxliteError::Storage(format!("failed to open tar {}: {}", tar_path.display(), e))
    })?;
    let mut archive = tar::Archive::new(tar_file);
    if let Ok(entries) = archive.entries() {
        let mut count = 0u32;
        let mut is_regular = false;
        for entry in entries {
            count += 1;
            if count > 1 {
                break;
            }
            if let Ok(e) = entry {
                is_regular = e.header().entry_type() == tar::EntryType::Regular;
            }
        }
        if count == 1 && is_regular {
            return Ok(ExtractionMode::FileToFile);
        }
    }
    Ok(ExtractionMode::IntoDirectory)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    // ── Helpers ───────────────────────────────────────────────────

    /// Pack `src` into a tar archive *file*, for the file-based [`unpack`]
    /// cases below.
    ///
    /// Test-only: production packs straight into a stream ([`pack_stream`]),
    /// so nothing outside these tests needs an archive on disk.
    async fn pack(src: PathBuf, tar_path: PathBuf, opts: PackContext) -> BoxliteResult<()> {
        tokio::task::spawn_blocking(move || {
            let tar_file = std::fs::File::create(&tar_path).map_err(|e| {
                BoxliteError::Storage(format!(
                    "failed to create tar {}: {}",
                    tar_path.display(),
                    e
                ))
            })?;
            pack_blocking(&src, tar_file, &opts)
        })
        .await
        .map_err(|e| BoxliteError::Storage(format!("pack task join error: {}", e)))?
    }

    fn uc(overwrite: bool, mkdir_parents: bool, force_directory: bool) -> UnpackContext {
        UnpackContext {
            overwrite,
            mkdir_parents,
            force_directory,
        }
    }

    fn default_unpack(overwrite: bool) -> UnpackContext {
        uc(overwrite, true, false)
    }

    fn default_pack() -> PackContext {
        PackContext {
            follow_symlinks: true,
            include_parent: true,
        }
    }

    /// Create a tar containing a single file with the given entry name and content.
    fn create_single_file_tar(tar_path: &Path, entry_name: &str, content: &[u8]) {
        let tar_file = std::fs::File::create(tar_path).unwrap();
        let mut builder = tar::Builder::new(tar_file);
        let mut header = tar::Header::new_gnu();
        header.set_size(content.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        builder
            .append_data(&mut header, entry_name, content)
            .unwrap();
        builder.finish().unwrap();
    }

    /// Create a tar carrying only file entries — no entry for any directory
    /// their names sit in, which is what plenty of tar writers emit.
    fn create_leaf_only_tar(tar_path: &Path, entry_names: &[&str]) {
        let tar_file = std::fs::File::create(tar_path).unwrap();
        let mut builder = tar::Builder::new(tar_file);
        for name in entry_names {
            let content = name.as_bytes();
            let mut header = tar::Header::new_gnu();
            header.set_size(content.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            builder.append_data(&mut header, name, content).unwrap();
        }
        builder.finish().unwrap();
    }

    /// Every path under `root`, directories included — what extraction left
    /// behind, read back off the disk rather than assumed.
    fn extracted_paths(root: &Path) -> Vec<PathBuf> {
        let mut found = Vec::new();
        let mut pending = vec![root.to_path_buf()];
        while let Some(dir) = pending.pop() {
            for entry in std::fs::read_dir(&dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    pending.push(path.clone());
                }
                found.push(path);
            }
        }
        found
    }

    /// Create a tar containing a directory with files inside.
    fn create_dir_tar(tar_path: &Path) {
        let tar_file = std::fs::File::create(tar_path).unwrap();
        let mut builder = tar::Builder::new(tar_file);

        let mut dir_header = tar::Header::new_gnu();
        dir_header.set_entry_type(tar::EntryType::Directory);
        dir_header.set_size(0);
        dir_header.set_mode(0o755);
        dir_header.set_cksum();
        builder
            .append_data(&mut dir_header, "mydir/", &[] as &[u8])
            .unwrap();

        let content = b"inside dir";
        let mut file_header = tar::Header::new_gnu();
        file_header.set_size(content.len() as u64);
        file_header.set_mode(0o644);
        file_header.set_cksum();
        builder
            .append_data(&mut file_header, "mydir/file.txt", &content[..])
            .unwrap();

        builder.finish().unwrap();
    }

    // ── pack: single file ────────────────────────────────────────

    #[tokio::test]
    async fn pack_single_file() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("hello.txt");
        std::fs::write(&src, b"hello").unwrap();

        let tar_path = tmp.path().join("out.tar");
        pack(
            src,
            tar_path.clone(),
            PackContext {
                follow_symlinks: true,
                include_parent: false,
            },
        )
        .await
        .unwrap();

        // Verify tar contains exactly one entry with the filename
        let tar_file = std::fs::File::open(&tar_path).unwrap();
        let mut archive = tar::Archive::new(tar_file);
        let entries: Vec<_> = archive.entries().unwrap().collect();
        assert_eq!(entries.len(), 1);
    }

    #[tokio::test]
    async fn pack_empty_file() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("empty.txt");
        std::fs::write(&src, b"").unwrap();

        let tar_path = tmp.path().join("out.tar");
        pack(
            src,
            tar_path.clone(),
            PackContext {
                follow_symlinks: true,
                include_parent: false,
            },
        )
        .await
        .unwrap();

        let dest = tmp.path().join("dest.txt");
        unpack(tar_path, dest.clone(), default_unpack(true))
            .await
            .unwrap();
        assert_eq!(std::fs::read(&dest).unwrap().len(), 0);
    }

    #[tokio::test]
    async fn pack_binary_content_fidelity() {
        let tmp = TempDir::new().unwrap();
        let data: Vec<u8> = (0..=255).collect();
        let src = tmp.path().join("binary.bin");
        std::fs::write(&src, &data).unwrap();

        let tar_path = tmp.path().join("out.tar");
        pack(
            src,
            tar_path.clone(),
            PackContext {
                follow_symlinks: true,
                include_parent: false,
            },
        )
        .await
        .unwrap();

        let dest = tmp.path().join("dest.bin");
        unpack(tar_path, dest.clone(), default_unpack(true))
            .await
            .unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), data);
    }

    // ── pack: directory with include_parent ───────────────────────

    #[tokio::test]
    async fn pack_dir_include_parent_true() {
        let tmp = TempDir::new().unwrap();
        let src_dir = tmp.path().join("mydir");
        std::fs::create_dir(&src_dir).unwrap();
        std::fs::write(src_dir.join("a.txt"), "aaa").unwrap();
        std::fs::write(src_dir.join("b.txt"), "bbb").unwrap();

        let tar_path = tmp.path().join("out.tar");
        pack(src_dir, tar_path.clone(), default_pack())
            .await
            .unwrap();

        let dest = tmp.path().join("dest");
        std::fs::create_dir(&dest).unwrap();
        unpack(tar_path, dest.clone(), default_unpack(true))
            .await
            .unwrap();

        // Files nested under mydir/
        assert_eq!(
            std::fs::read_to_string(dest.join("mydir").join("a.txt")).unwrap(),
            "aaa"
        );
        assert_eq!(
            std::fs::read_to_string(dest.join("mydir").join("b.txt")).unwrap(),
            "bbb"
        );
    }

    #[tokio::test]
    async fn pack_dir_include_parent_false_flattens() {
        let tmp = TempDir::new().unwrap();
        let src_dir = tmp.path().join("flatdir");
        std::fs::create_dir(&src_dir).unwrap();
        std::fs::write(src_dir.join("f.txt"), "flat").unwrap();

        let tar_path = tmp.path().join("out.tar");
        pack(
            src_dir,
            tar_path.clone(),
            PackContext {
                follow_symlinks: true,
                include_parent: false,
            },
        )
        .await
        .unwrap();

        let dest = tmp.path().join("dest");
        std::fs::create_dir(&dest).unwrap();
        unpack(tar_path, dest.clone(), uc(true, false, true))
            .await
            .unwrap();

        // File directly in dest, not under flatdir/
        assert_eq!(std::fs::read_to_string(dest.join("f.txt")).unwrap(), "flat");
    }

    #[tokio::test]
    async fn pack_empty_directory() {
        let tmp = TempDir::new().unwrap();
        let src_dir = tmp.path().join("emptydir");
        std::fs::create_dir(&src_dir).unwrap();

        let tar_path = tmp.path().join("out.tar");
        pack(src_dir, tar_path.clone(), default_pack())
            .await
            .unwrap();

        let dest = tmp.path().join("dest");
        std::fs::create_dir(&dest).unwrap();
        unpack(tar_path, dest.clone(), default_unpack(true))
            .await
            .unwrap();
        assert!(dest.join("emptydir").is_dir());
    }

    #[tokio::test]
    async fn pack_nested_directory() {
        let tmp = TempDir::new().unwrap();
        let src_dir = tmp.path().join("deep");
        std::fs::create_dir_all(src_dir.join("a").join("b").join("c")).unwrap();
        std::fs::write(
            src_dir.join("a").join("b").join("c").join("file.txt"),
            "deep",
        )
        .unwrap();
        std::fs::write(src_dir.join("top.txt"), "top").unwrap();

        let tar_path = tmp.path().join("out.tar");
        pack(src_dir, tar_path.clone(), default_pack())
            .await
            .unwrap();

        let dest = tmp.path().join("dest");
        std::fs::create_dir(&dest).unwrap();
        unpack(tar_path, dest.clone(), default_unpack(true))
            .await
            .unwrap();

        assert_eq!(
            std::fs::read_to_string(
                dest.join("deep")
                    .join("a")
                    .join("b")
                    .join("c")
                    .join("file.txt")
            )
            .unwrap(),
            "deep"
        );
        assert_eq!(
            std::fs::read_to_string(dest.join("deep").join("top.txt")).unwrap(),
            "top"
        );
    }

    // ── pack: symlinks ───────────────────────────────────────────

    #[tokio::test]
    async fn pack_follow_symlinks_false_preserves_link() {
        let tmp = TempDir::new().unwrap();
        let src_dir = tmp.path().join("linkdir");
        std::fs::create_dir(&src_dir).unwrap();
        std::fs::write(src_dir.join("target.txt"), "target content").unwrap();
        std::os::unix::fs::symlink("target.txt", src_dir.join("link.txt")).unwrap();

        let tar_path = tmp.path().join("out.tar");
        pack(
            src_dir,
            tar_path.clone(),
            PackContext {
                follow_symlinks: false,
                include_parent: true,
            },
        )
        .await
        .unwrap();

        let dest = tmp.path().join("dest");
        std::fs::create_dir(&dest).unwrap();
        unpack(tar_path, dest.clone(), default_unpack(true))
            .await
            .unwrap();

        let link_path = dest.join("linkdir").join("link.txt");
        assert!(link_path
            .symlink_metadata()
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(
            std::fs::read_link(&link_path).unwrap().to_str().unwrap(),
            "target.txt"
        );
    }

    #[tokio::test]
    async fn pack_follow_symlinks_true_dereferences() {
        let tmp = TempDir::new().unwrap();
        let src_dir = tmp.path().join("derefdir");
        std::fs::create_dir(&src_dir).unwrap();
        std::fs::write(src_dir.join("target.txt"), "deref content").unwrap();
        std::os::unix::fs::symlink("target.txt", src_dir.join("link.txt")).unwrap();

        let tar_path = tmp.path().join("out.tar");
        pack(
            src_dir,
            tar_path.clone(),
            PackContext {
                follow_symlinks: true,
                include_parent: true,
            },
        )
        .await
        .unwrap();

        let dest = tmp.path().join("dest");
        std::fs::create_dir(&dest).unwrap();
        unpack(tar_path, dest.clone(), default_unpack(true))
            .await
            .unwrap();

        let link_path = dest.join("derefdir").join("link.txt");
        // Should be a regular file, not a symlink
        assert!(link_path.is_file());
        assert!(!link_path
            .symlink_metadata()
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(
            std::fs::read_to_string(&link_path).unwrap(),
            "deref content"
        );
    }

    // ── pack: error cases ────────────────────────────────────────

    #[tokio::test]
    async fn pack_nonexistent_source_errors() {
        let tmp = TempDir::new().unwrap();
        let tar_path = tmp.path().join("out.tar");
        let result = pack(tmp.path().join("does-not-exist"), tar_path, default_pack()).await;
        assert!(result.is_err());
    }

    // ── unpack: detection modes ──────────────────────────────────

    #[tokio::test]
    async fn unpack_single_file_to_nonexistent_path_uses_file_mode() {
        let tmp = TempDir::new().unwrap();
        let tar_path = tmp.path().join("single.tar");
        create_single_file_tar(&tar_path, "hello.txt", b"hello");

        let dest = tmp.path().join("output.txt");
        unpack(tar_path, dest.clone(), default_unpack(true))
            .await
            .unwrap();
        assert!(dest.is_file());
        assert_eq!(std::fs::read_to_string(&dest).unwrap(), "hello");
    }

    #[tokio::test]
    async fn unpack_single_file_to_existing_dir_uses_dir_mode() {
        let tmp = TempDir::new().unwrap();
        let tar_path = tmp.path().join("single.tar");
        create_single_file_tar(&tar_path, "hello.txt", b"hello");

        let dest = tmp.path().to_path_buf(); // existing directory
        unpack(tar_path, dest.clone(), default_unpack(true))
            .await
            .unwrap();
        assert!(dest.join("hello.txt").is_file());
    }

    #[tokio::test]
    async fn unpack_trailing_slash_forces_dir_mode() {
        let tmp = TempDir::new().unwrap();
        let tar_path = tmp.path().join("single.tar");
        create_single_file_tar(&tar_path, "hello.txt", b"hello");

        let dest = tmp.path().join("dirout");
        std::fs::create_dir(&dest).unwrap();
        let dest_with_slash = PathBuf::from(format!("{}/", dest.display()));
        unpack(tar_path, dest_with_slash, default_unpack(true))
            .await
            .unwrap();
        assert!(dest.join("hello.txt").is_file());
    }

    #[tokio::test]
    async fn unpack_multi_entry_tar_uses_dir_mode() {
        let tmp = TempDir::new().unwrap();
        let tar_path = tmp.path().join("multi.tar");
        create_dir_tar(&tar_path);

        let dest = tmp.path().join("output");
        std::fs::create_dir(&dest).unwrap();
        unpack(tar_path, dest.clone(), default_unpack(true))
            .await
            .unwrap();

        assert!(dest.join("mydir").join("file.txt").is_file());
        assert_eq!(
            std::fs::read_to_string(dest.join("mydir").join("file.txt")).unwrap(),
            "inside dir"
        );
    }

    #[tokio::test]
    async fn unpack_single_dir_entry_uses_dir_mode() {
        let tmp = TempDir::new().unwrap();
        let tar_path = tmp.path().join("dir_only.tar");

        let tar_file = std::fs::File::create(&tar_path).unwrap();
        let mut builder = tar::Builder::new(tar_file);
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::Directory);
        header.set_size(0);
        header.set_mode(0o755);
        header.set_cksum();
        builder
            .append_data(&mut header, "somedir/", &[] as &[u8])
            .unwrap();
        builder.finish().unwrap();

        let dest = tmp.path().join("output");
        unpack(tar_path, dest.clone(), default_unpack(true))
            .await
            .unwrap();
        assert!(dest.join("somedir").is_dir());
    }

    #[tokio::test]
    async fn unpack_empty_tar_uses_dir_mode() {
        let tmp = TempDir::new().unwrap();
        let tar_path = tmp.path().join("empty.tar");

        let tar_file = std::fs::File::create(&tar_path).unwrap();
        let builder = tar::Builder::new(tar_file);
        builder.into_inner().unwrap();

        let dest = tmp.path().join("output");
        // Empty tar + dir mode + mkdir_parents → creates empty directory
        unpack(tar_path, dest.clone(), default_unpack(true))
            .await
            .unwrap();
        assert!(dest.is_dir());
    }

    // ── unpack: force_directory ──────────────────────────────────

    #[tokio::test]
    async fn force_directory_overrides_single_file_detection() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("file.txt");
        std::fs::write(&src, b"data").unwrap();

        let tar_path = tmp.path().join("out.tar");
        pack(
            src,
            tar_path.clone(),
            PackContext {
                follow_symlinks: true,
                include_parent: false,
            },
        )
        .await
        .unwrap();

        let dest = tmp.path().join("dir_dest");
        std::fs::create_dir(&dest).unwrap();
        unpack(tar_path, dest.clone(), uc(true, false, true))
            .await
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(dest.join("file.txt")).unwrap(),
            "data"
        );
    }

    // ── unpack: overwrite ────────────────────────────────────────

    #[tokio::test]
    async fn unpack_overwrite_true_replaces_file() {
        let tmp = TempDir::new().unwrap();
        let tar_path = tmp.path().join("file.tar");
        create_single_file_tar(&tar_path, "data.txt", b"new content");

        let dest = tmp.path().join("data.txt");
        std::fs::write(&dest, b"old content").unwrap();

        unpack(tar_path, dest.clone(), default_unpack(true))
            .await
            .unwrap();
        assert_eq!(std::fs::read_to_string(&dest).unwrap(), "new content");
    }

    #[tokio::test]
    async fn unpack_overwrite_false_rejects_existing_file() {
        let tmp = TempDir::new().unwrap();
        let tar_path = tmp.path().join("file.tar");
        create_single_file_tar(&tar_path, "data.txt", b"new content");

        let dest = tmp.path().join("data.txt");
        std::fs::write(&dest, b"old content").unwrap();

        let result = unpack(tar_path, dest.clone(), default_unpack(false)).await;
        assert!(result.is_err());
        assert_eq!(std::fs::read_to_string(&dest).unwrap(), "old content");
    }

    #[tokio::test]
    async fn unpack_overwrite_false_rejects_existing_dir() {
        let tmp = TempDir::new().unwrap();
        let tar_path = tmp.path().join("dir.tar");
        create_dir_tar(&tar_path);

        let dest = tmp.path().join("output");
        std::fs::create_dir(&dest).unwrap();

        let result = unpack(tar_path, dest, uc(false, false, false)).await;
        assert!(result.is_err());
    }

    // ── unpack: mkdir_parents ────────────────────────────────────

    #[tokio::test]
    async fn unpack_mkdir_parents_creates_parent_dirs_for_file() {
        let tmp = TempDir::new().unwrap();
        let tar_path = tmp.path().join("file.tar");
        create_single_file_tar(&tar_path, "data.txt", b"content");

        let dest = tmp.path().join("a").join("b").join("c").join("data.txt");
        unpack(tar_path, dest.clone(), default_unpack(true))
            .await
            .unwrap();

        assert!(dest.is_file());
        assert_eq!(std::fs::read_to_string(&dest).unwrap(), "content");
    }

    #[tokio::test]
    async fn unpack_mkdir_parents_creates_dest_dir() {
        let tmp = TempDir::new().unwrap();
        let tar_path = tmp.path().join("dir.tar");
        create_dir_tar(&tar_path);

        let dest = tmp.path().join("x").join("y").join("z");
        unpack(tar_path, dest.clone(), uc(true, true, true))
            .await
            .unwrap();
        assert!(dest.join("mydir").join("file.txt").is_file());
    }

    /// A failed extraction must carry the errno, not just the operation.
    ///
    /// `tar`'s own `Display` stops at "failed to create `<path>`", which is how
    /// a real diagnosis once stalled: the message named the path and hid that
    /// the cause was ENOTDIR. The reason has to survive into `BoxliteError`.
    ///
    /// The failure must originate *inside* `archive.unpack`, so `dest` is created
    /// up front: that makes `if !dest.exists()` false and skips the arm's own
    /// `create_dir_all`, whose `map_err` already formats the io::Error and would
    /// mask what is being tested.
    ///
    /// `create_dir_tar` carries `mydir/file.txt`. tar defers directory entries to
    /// the end of extraction, so the file is unpacked first, and `ensure_dir_created`
    /// is a no-op because `dest/mydir` already exists — as a regular file. The
    /// failure is therefore creating the *file* beneath a non-directory, which the
    /// kernel refuses with ENOTDIR.
    #[tokio::test]
    async fn unpack_error_carries_the_cause_not_just_the_operation() {
        let tmp = TempDir::new().unwrap();
        let tar_path = tmp.path().join("dir.tar");
        create_dir_tar(&tar_path);

        let dest = tmp.path().join("dest");
        std::fs::create_dir(&dest).unwrap();
        // `mydir` must be a directory for the archive to extract; make it a file.
        std::fs::write(dest.join("mydir"), b"blocker").unwrap();

        let err = unpack(tar_path, dest, uc(true, true, true))
            .await
            .expect_err("extracting beneath a regular file must fail");

        let msg = err.to_string();
        // Reverted, this reads "failed to unpack `…/dest/mydir/file.txt`" and stops
        // there — the operation named, the reason gone. ENOTDIR is 20 on both Linux
        // and macOS; accept either rendering of it.
        assert!(
            msg.contains("Not a directory") || msg.contains("os error 20"),
            "error must name ENOTDIR, not just the operation, got: {msg}"
        );
    }

    // ── entry_paths ──────────────────────────────────────────────

    /// The walk must not run on the caller's async worker.
    ///
    /// It reads the whole archive (see [`entry_paths`]), and the caller
    /// chooses how big that is — `boxlite-guest` accepts uploads up to
    /// 512 MiB. Run inline on the guest agent's runtime, one such copy stalls
    /// every other RPC sharing that worker — exec output, health checks — for
    /// as long as the read takes.
    ///
    /// A `current_thread` runtime makes the difference observable: one worker,
    /// so `ticker` can only be polled if the walk yields. Called inline the
    /// walk never yields and `ticker` is still pending when it returns.
    #[tokio::test(flavor = "current_thread")]
    async fn entry_paths_yields_the_worker_while_it_walks() {
        let tmp = TempDir::new().unwrap();
        let tar_path = tmp.path().join("big.tar");
        // Large enough that the read cannot plausibly finish between handing
        // the closure to the blocking pool and the first poll of its handle,
        // so a passing assertion means the yield happened, not that it raced.
        create_single_file_tar(&tar_path, "payload.bin", &vec![0u8; 4 << 20]);

        let ticker = tokio::spawn(async {});
        let paths = entry_paths(tar_path).await.unwrap();

        assert_eq!(paths, vec![PathBuf::from("payload.bin")]);
        assert!(
            ticker.is_finished(),
            "the walk held the only worker for the whole archive"
        );
    }

    /// The tar is caller-supplied. An entry that is absolute or climbs out is
    /// skipped by extraction, so it must be skipped here too — otherwise
    /// `dest.join(entry)` escapes, and in the guest that hands a root-owned
    /// chown a file this copy never created.
    #[test]
    fn entry_paths_that_escape_the_destination_are_dropped() {
        assert_eq!(sanitize_entry_path(Path::new("../../etc/shadow")), None);
        assert_eq!(sanitize_entry_path(Path::new("a/../../b")), None);
        assert_eq!(sanitize_entry_path(Path::new("..")), None);
    }

    /// An absolute entry keeps its tail, matching extraction stripping the
    /// leading `/` rather than refusing the entry outright.
    #[test]
    fn absolute_entry_paths_are_made_relative() {
        assert_eq!(
            sanitize_entry_path(Path::new("/etc/shadow")),
            Some(PathBuf::from("etc/shadow"))
        );
    }

    #[test]
    fn ordinary_entry_paths_survive() {
        assert_eq!(
            sanitize_entry_path(Path::new("./dir/file.txt")),
            Some(PathBuf::from("dir/file.txt"))
        );
        assert_eq!(sanitize_entry_path(Path::new(".")), None);
    }

    /// The names must be the ones extraction will actually create, or the
    /// guest's mount check and its ownership hand-off both reason about a
    /// layout that never existed.
    #[tokio::test]
    async fn entry_paths_match_what_extraction_creates() {
        let tmp = TempDir::new().unwrap();
        let tar_path = tmp.path().join("dir.tar");
        create_dir_tar(&tar_path);

        let names = entry_paths(tar_path.clone()).await.unwrap();

        let dest = tmp.path().join("dest");
        unpack(tar_path, dest.clone(), uc(true, true, true))
            .await
            .unwrap();
        for name in &names {
            assert!(
                dest.join(name).exists(),
                "{} was named but not extracted",
                name.display()
            );
        }
        assert_eq!(
            names,
            vec![PathBuf::from("mydir"), PathBuf::from("mydir/file.txt")]
        );
    }

    /// The other half of the same contract: nothing extraction creates may go
    /// unnamed.
    ///
    /// A tar need not carry an entry for a directory it puts files in, and
    /// plenty of writers emit only leaves — `PUT /boxes/{id}/files` takes any
    /// `application/x-tar` the caller built. Extraction still conjures the
    /// directories (`tar-0.4.45/src/entry.rs`, `ensure_dir_created`), so a list
    /// that stops at the leaves under-reports what this copy made, and the
    /// guest hands the box user only what this list names: the file arrives
    /// owned by the workload inside a directory still owned by root, which is
    /// POL-304 again one level up.
    #[tokio::test]
    async fn entry_paths_name_the_directories_a_nested_entry_implies() {
        let tmp = TempDir::new().unwrap();
        let tar_path = tmp.path().join("leaves.tar");
        create_leaf_only_tar(&tar_path, &["app/main.py", "app/util.py"]);

        let names = entry_paths(tar_path.clone()).await.unwrap();

        let dest = tmp.path().join("dest");
        unpack(tar_path, dest.clone(), uc(true, true, false))
            .await
            .unwrap();

        let named: std::collections::HashSet<PathBuf> =
            names.iter().map(|rel| dest.join(rel)).collect();
        for created in extracted_paths(&dest) {
            assert!(
                named.contains(&created),
                "{} was created by extraction but never named; named: {:?}",
                created.display(),
                names
            );
        }
    }

    /// A truncated archive must not fail the walk: extraction reads the same
    /// bytes moments later and reports the real parse error.
    #[tokio::test]
    async fn entry_paths_on_a_malformed_archive_is_empty_not_an_error() {
        let tmp = TempDir::new().unwrap();
        let tar_path = tmp.path().join("junk.tar");
        std::fs::write(&tar_path, b"not a tar at all").unwrap();

        assert_eq!(entry_paths(tar_path).await.unwrap(), Vec::<PathBuf>::new());
    }

    #[tokio::test]
    async fn unpack_no_mkdir_parents_errors_on_missing_parent() {
        let tmp = TempDir::new().unwrap();
        let tar_path = tmp.path().join("file.tar");
        create_single_file_tar(&tar_path, "data.txt", b"content");

        let dest = tmp.path().join("nonexistent").join("data.txt");
        let result = unpack(tar_path, dest, uc(true, false, false)).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn unpack_no_mkdir_parents_errors_on_missing_dest_dir() {
        let tmp = TempDir::new().unwrap();
        let tar_path = tmp.path().join("dir.tar");
        create_dir_tar(&tar_path);

        let dest = tmp.path().join("nonexistent");
        let result = unpack(tar_path, dest, uc(true, false, true)).await;
        assert!(result.is_err());
    }

    // ── roundtrip: pack + unpack ─────────────────────────────────

    #[tokio::test]
    async fn roundtrip_single_file() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("hello.txt");
        std::fs::write(&src, b"hello").unwrap();

        let tar_path = tmp.path().join("out.tar");
        pack(
            src,
            tar_path.clone(),
            PackContext {
                follow_symlinks: true,
                include_parent: false,
            },
        )
        .await
        .unwrap();

        let dest = tmp.path().join("dest.txt");
        unpack(tar_path, dest.clone(), default_unpack(true))
            .await
            .unwrap();
        assert_eq!(std::fs::read_to_string(&dest).unwrap(), "hello");
    }

    #[tokio::test]
    async fn roundtrip_dir_with_parent() {
        let tmp = TempDir::new().unwrap();
        let src_dir = tmp.path().join("src");
        std::fs::create_dir(&src_dir).unwrap();
        std::fs::write(src_dir.join("hello.txt"), b"hello").unwrap();

        let tar_path = tmp.path().join("out.tar");
        pack(src_dir, tar_path.clone(), default_pack())
            .await
            .unwrap();

        let dest_dir = tmp.path().join("dest");
        std::fs::create_dir(&dest_dir).unwrap();
        unpack(tar_path, dest_dir.clone(), default_unpack(true))
            .await
            .unwrap();

        assert_eq!(
            std::fs::read_to_string(dest_dir.join("src").join("hello.txt")).unwrap(),
            "hello"
        );
    }

    /// Regression test for #238: copy_in creates directory when destination is a file path.
    #[tokio::test]
    async fn issue_238_file_to_file_path_not_directory() {
        let tmp = TempDir::new().unwrap();
        let src_file = tmp.path().join("script.py");
        std::fs::write(&src_file, b"print('hello')\n").unwrap();

        let tar_path = tmp.path().join("issue238.tar");
        pack(
            src_file,
            tar_path.clone(),
            PackContext {
                follow_symlinks: true,
                include_parent: false,
            },
        )
        .await
        .unwrap();

        let workspace = tmp.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let dest_file = workspace.join("script.py");
        unpack(tar_path, dest_file.clone(), default_unpack(true))
            .await
            .unwrap();

        assert!(
            dest_file.is_file(),
            "script.py should be a file (issue #238)"
        );
        assert!(
            !dest_file.is_dir(),
            "script.py must NOT be a directory (issue #238)"
        );
        assert_eq!(
            std::fs::read_to_string(&dest_file).unwrap(),
            "print('hello')\n"
        );
    }

    #[tokio::test]
    async fn roundtrip_file_to_existing_dir_extracts_inside() {
        let tmp = TempDir::new().unwrap();
        let src_file = tmp.path().join("source.py");
        std::fs::write(&src_file, b"print('hello')").unwrap();
        let tar_path = tmp.path().join("file.tar");
        pack(
            src_file,
            tar_path.clone(),
            PackContext {
                follow_symlinks: true,
                include_parent: false,
            },
        )
        .await
        .unwrap();

        let dest_dir = tmp.path().join("workspace");
        std::fs::create_dir(&dest_dir).unwrap();
        unpack(tar_path, dest_dir.clone(), default_unpack(true))
            .await
            .unwrap();

        let extracted = dest_dir.join("source.py");
        assert!(extracted.is_file());
        assert_eq!(
            std::fs::read_to_string(&extracted).unwrap(),
            "print('hello')"
        );
    }

    #[tokio::test]
    async fn roundtrip_filename_with_spaces() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("my file.txt");
        std::fs::write(&src, "spaces\n").unwrap();

        let tar_path = tmp.path().join("out.tar");
        pack(
            src,
            tar_path.clone(),
            PackContext {
                follow_symlinks: true,
                include_parent: false,
            },
        )
        .await
        .unwrap();

        let dest = tmp.path().join("my file out.txt");
        unpack(tar_path, dest.clone(), default_unpack(true))
            .await
            .unwrap();
        assert_eq!(std::fs::read_to_string(&dest).unwrap(), "spaces\n");
    }

    // ── streaming pack/unpack round-trips ────────────────────────

    // ── streaming failure-path machinery ──────────────────────────────

    /// A writer that panics on its first write. Subsequent writes succeed so
    /// the drop-time `Builder::finish` (tar::Builder::Drop calls finish, which
    /// writes the trailer) cannot double-panic during the unwind.
    struct PanicWriter(bool);
    impl Write for PanicWriter {
        fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
            if self.0 {
                self.0 = false;
                panic!("injected pack panic")
            }
            Ok(0)
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn pack_task_body_surfaces_panic_as_error() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("p.txt");
        std::fs::write(&src, b"x").unwrap();

        let result = pack_task_body(
            &src,
            PanicWriter(true),
            &PackContext {
                follow_symlinks: true,
                include_parent: false,
            },
        );
        assert!(
            matches!(result, Err(BoxliteError::Internal(_))),
            "a panicking pack must surface as Internal, got {result:?}"
        );
    }

    /// PipeWriter must deliver sub-chunk pending bytes on flush — the final
    /// partial chunk of a small archive rides on this, not on Drop. Plain
    /// (non-runtime) thread: `blocking_send` panics inside a tokio runtime.
    #[test]
    fn pipe_writer_flush_delivers_pending_bytes() {
        let (tx, mut rx) = mpsc::channel(4);
        let mut writer = PipeWriter {
            tx,
            pending: Vec::with_capacity(COPY_CHUNK_SIZE),
        };
        writer.write_all(b"tail bytes").unwrap();
        writer.flush().unwrap();
        drop(writer);

        let chunk = rx
            .blocking_recv()
            .expect("flush must deliver the chunk")
            .unwrap();
        assert_eq!(chunk, b"tail bytes");
        assert!(
            rx.blocking_recv().is_none(),
            "dropped sender must signal EOF"
        );
    }

    /// A consumer that dropped the stream must surface as BrokenPipe from
    /// flush — the pack task then reports a terminal Err instead of silently
    /// truncating. Small writes stay buffered, so the failure only shows at
    /// flush time (which is exactly what pack_blocking's explicit flush
    /// after finish exists to observe).
    #[test]
    fn pipe_writer_reports_broken_pipe_on_dropped_consumer() {
        let (tx, rx) = mpsc::channel(4);
        drop(rx); // consumer gone before any send
        let mut writer = PipeWriter {
            tx,
            pending: Vec::with_capacity(COPY_CHUNK_SIZE),
        };
        writer.write_all(b"lost bytes").unwrap(); // buffered, no send yet
        let err = writer.flush().unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::BrokenPipe);
    }

    #[tokio::test]
    async fn stream_pack_reports_source_is_dir() {
        let tmp = TempDir::new().unwrap();

        let file = tmp.path().join("f.txt");
        std::fs::write(&file, b"x").unwrap();
        let (is_dir, _stream) = pack_stream(file, default_pack()).await.unwrap();
        assert!(!is_dir);

        let dir = tmp.path().join("d");
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(dir.join("f.txt"), b"x").unwrap();
        let (is_dir, _stream) = pack_stream(dir, default_pack()).await.unwrap();
        assert!(is_dir);
    }

    #[tokio::test]
    async fn stream_pack_nonexistent_source_errors() {
        let tmp = TempDir::new().unwrap();
        let result = pack_stream(tmp.path().join("does-not-exist"), default_pack()).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn stream_roundtrip_single_file() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("hello.txt");
        std::fs::write(&src, b"streaming hello").unwrap();

        let (source_is_dir, stream) = pack_stream(
            src,
            PackContext {
                follow_symlinks: true,
                include_parent: false,
            },
        )
        .await
        .unwrap();
        assert!(!source_is_dir);

        let dest = tmp.path().join("dest.txt");
        let report = unpack_stream(Box::pin(stream), dest.clone(), uc(true, true, false))
            .await
            .unwrap();
        assert_eq!(std::fs::read_to_string(&dest).unwrap(), "streaming hello");
        assert_eq!(
            report.entry_paths,
            vec![PathBuf::from("hello.txt")],
            "streamed extraction must report the entry it created"
        );
    }

    #[tokio::test]
    async fn stream_roundtrip_dir_tree() {
        let tmp = TempDir::new().unwrap();
        let src_dir = tmp.path().join("s");
        std::fs::create_dir_all(src_dir.join("sub")).unwrap();
        std::fs::write(src_dir.join("a.txt"), "aaa").unwrap();
        std::fs::write(src_dir.join("sub").join("b.txt"), "bbb").unwrap();

        let (source_is_dir, stream) = pack_stream(
            src_dir,
            PackContext {
                follow_symlinks: true,
                include_parent: true,
            },
        )
        .await
        .unwrap();
        assert!(source_is_dir);

        let dest = tmp.path().join("out");
        let report = unpack_stream(Box::pin(stream), dest.clone(), uc(true, true, true))
            .await
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(dest.join("s").join("a.txt")).unwrap(),
            "aaa"
        );
        assert_eq!(
            std::fs::read_to_string(dest.join("s").join("sub").join("b.txt")).unwrap(),
            "bbb"
        );
        for expected in ["s", "s/a.txt", "s/sub", "s/sub/b.txt"] {
            assert!(
                report.entry_paths.contains(&PathBuf::from(expected)),
                "created paths missing {expected}: {:?}",
                report.entry_paths
            );
        }
    }
}
