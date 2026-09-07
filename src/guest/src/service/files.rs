#![cfg(target_os = "linux")]
//! Files service implementation.
//!
//! Provides tar-based upload/download between host and the single container
//! running inside the guest.

use crate::service::server::GuestServer;
use boxlite_shared::{
    files_server::Files, BoxByteStream, DownloadChunk, DownloadRequest, UploadChunk, UploadResponse,
};
use futures::StreamExt;
use nix::fcntl::OFlag;
use std::collections::HashSet;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::fs::File;
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status, Streaming};
use tracing::{info, warn};

const MAX_UPLOAD_BYTES: u64 = 512 * 1024 * 1024; // 512 MiB safety cap

/// A staged upload, deleted whenever it goes out of scope.
///
/// The guest's temp dir is RAM-backed, and a request leaves through half a
/// dozen `?`s between creating this file and finishing with it — the size cap,
/// a write error, a mount refusal, a failed extraction. Deleting at the end of
/// the happy path only meant every one of those stranded up to
/// [`MAX_UPLOAD_BYTES`] of guest memory for the life of the VM.
///
/// Hand-rolled rather than `tempfile::TempPath`: `tempfile` is a dev-dependency
/// here, and this is not worth adding to what ships inside the VM.
struct StagedTar(PathBuf);

impl StagedTar {
    fn new(path: PathBuf) -> Self {
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for StagedTar {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Open `rel` under `root`, with every symlink confined to `root`.
///
/// `fchownat(None, "/abs/path", …, AT_SYMLINK_NOFOLLOW)` guards only the final
/// component; every directory above it is resolved by the kernel, symlinks
/// followed. So a workload that swaps a directory this copy just extracted for
/// a symlink — after extraction, while the ownership hand-off is still working
/// through its list — redirects the chown to whatever the link names. Naming a
/// path and then acting on it is two operations, and the box gets to move the
/// target in between; runc's CVE-2025-52565 is the same shape, and its fix was
/// likewise to stop passing paths and start carrying descriptors.
///
/// `RESOLVE_IN_ROOT` is what makes the descriptor safe to chown through: the
/// kernel resolves the whole path in one step and reinterprets every absolute
/// symlink and `..` against `root`, so a swapped ancestor lands back inside the
/// rootfs instead of outside it. Confining rather than refusing is deliberate —
/// images ship symlinked directories (`/lib -> usr/lib` on `python:3-slim`,
/// `/var/run -> ../run` on `redis:7.4-alpine`), and refusing those would leave
/// a copy to `/lib/x` extracted but still root-owned, which is the ownership
/// bug this hand-off exists to fix.
///
/// `O_PATH` because this fd is only ever a resolution anchor — never read,
/// never written. `openat2` needs 5.6 and the guest kernel is vendored and
/// pinned well past it (`src/deps/libkrun-sys/vendor/libkrunfw/Makefile`), so
/// its absence is not a case this has to carry.
fn open_dir_in_root(root: RawFd, rel: &Path) -> nix::Result<OwnedFd> {
    let how = nix::fcntl::OpenHow::new()
        .flags(OFlag::O_PATH | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC)
        .resolve(nix::fcntl::ResolveFlag::RESOLVE_IN_ROOT);

    // `openat2` rejects an empty path, so a target sitting directly in the
    // rootfs anchors on the root itself.
    let path = if rel.as_os_str().is_empty() {
        Path::new(".")
    } else {
        rel
    };

    // SAFETY: `openat2` returns a fresh descriptor this scope owns; wrapping it
    // immediately is what closes it on every exit.
    Ok(unsafe { OwnedFd::from_raw_fd(nix::fcntl::openat2(root, path, how)?) })
}

/// Give one path inside the rootfs to `uid:gid`, or refuse it.
///
/// The whole hand-off is this, once per target: prove the path is inside the
/// boundary, reach its parent with every link confined to the rootfs, and
/// change the leaf through that parent's descriptor. Kept as one function so
/// the composition — and not merely [`open_dir_in_root`] underneath it — is
/// what the tests hold.
///
/// What this bounds and what it does not: a swapped ancestor can no longer
/// steer the chown out of the rootfs, but it can still steer it onto another
/// path *within* the rootfs, so a workload can spend its own copy's hand-off on
/// a file of its choosing inside its own filesystem. `RESOLVE_BENEATH` would
/// refuse that too, and cannot be used: images ship absolute symlinked
/// directories (`/var/run -> /run` on `python:3-slim`), which `BENEATH` rejects
/// as an escape. Bounded to the box, in exchange for images working, is the
/// trade — and it is why nothing here treats the resolved path as evidence of
/// what the copy wrote.
///
/// `EXDEV` for a target outside the rootfs: [`DestBefore::created`] filters
/// those lexically already, so reaching here means the two disagree, and the
/// honest answer is to refuse rather than to chown on a lexical say-so.
fn chown_within_rootfs(
    anchor: RawFd,
    rootfs_root: &Path,
    target: &Path,
    uid: u32,
    gid: u32,
) -> nix::Result<()> {
    let rel = target
        .strip_prefix(rootfs_root)
        .map_err(|_| nix::errno::Errno::EXDEV)?;
    let leaf = rel.file_name().ok_or(nix::errno::Errno::EINVAL)?;
    let parent = open_dir_in_root(anchor, rel.parent().unwrap_or(Path::new("")))?;

    nix::unistd::fchownat(
        Some(parent.as_raw_fd()),
        Path::new(leaf),
        Some(nix::unistd::Uid::from_raw(uid)),
        Some(nix::unistd::Gid::from_raw(gid)),
        nix::fcntl::AtFlags::AT_SYMLINK_NOFOLLOW,
    )
}

/// Ancestors of `path`, outermost first, that do not exist yet.
///
/// Sampled before extraction so the directories `mkdir_parents` conjures can be
/// told apart from ones the image already shipped.
fn missing_ancestors(path: &Path) -> Vec<PathBuf> {
    let mut missing: Vec<PathBuf> = path
        .ancestors()
        .skip(1)
        .take_while(|ancestor| !ancestor.exists())
        .map(Path::to_path_buf)
        .collect();
    missing.reverse();
    missing
}

/// What the destination looked like *before* anything was extracted onto it.
///
/// Sampled up front because afterwards there is no telling a directory this
/// copy created from one the image already shipped — and only the former is
/// ours to hand to the box user.
struct DestBefore {
    /// Ancestors of the destination that `mkdir_parents` had to conjure,
    /// outermost first.
    created_dirs: Vec<PathBuf>,
    /// The destination itself was already there.
    dest_existed: bool,
    /// Absolute paths of archive entries that were already directories.
    ///
    /// A set, not a list: [`Self::created`] tests every target against it, and
    /// the caller sizes both — as a list, re-copying a large tree over itself
    /// made that scan `entries × existing directories`.
    existing_dirs: HashSet<PathBuf>,
}

impl DestBefore {
    /// The streamed-arm variant: all facts are captured while they still
    /// describe the tree before extraction creates each path.
    fn recorded(
        created_dirs: Vec<PathBuf>,
        dest_existed: bool,
        existing_dirs: HashSet<PathBuf>,
    ) -> Self {
        Self {
            created_dirs,
            dest_existed,
            existing_dirs,
        }
    }

    /// Sample off the async worker: one `stat` per archive entry, and the
    /// caller chooses how many entries there are — the same reason
    /// [`boxlite_shared::tar::entry_paths`] runs on a blocking thread.
    #[allow(clippy::result_large_err)]
    async fn sample(dest_root: PathBuf, entry_paths: Arc<[PathBuf]>) -> Result<Self, Status> {
        tokio::task::spawn_blocking(move || Self::sample_blocking(&dest_root, &entry_paths))
            .await
            .map_err(|e| Status::internal(format!("sample task join error: {e}")))
    }

    fn sample_blocking(dest_root: &Path, entry_paths: &[PathBuf]) -> Self {
        Self {
            created_dirs: missing_ancestors(dest_root),
            dest_existed: dest_root.exists(),
            existing_dirs: entry_paths
                .iter()
                .map(|rel| dest_root.join(rel))
                .filter(|path| path.is_dir())
                .collect(),
        }
    }

    /// Absolute paths this upload created — exactly the set that may change
    /// owner. Reads `dest_root` as it stands *after* extraction, so a
    /// destination the copy conjured is classified by what it became.
    ///
    /// Every entry the archive carried, the parent directories extraction had
    /// to make, and the destination itself when we made it. Directories the
    /// image already shipped are left alone whether the request names one
    /// (`copy_in("./x", "/usr/local/bin/")`) or the archive does
    /// (`copy_in("./dist", "/usr/local")` carrying `bin/tool`) — handing either
    /// to the box user is a permission change nobody asked for. Files are
    /// always ours, existing or not: extraction just overwrote them.
    fn created(&self, dest_root: &Path, entry_paths: &[PathBuf]) -> Vec<PathBuf> {
        let dest_is_dir = dest_root.is_dir();
        let mut targets = self.created_dirs.clone();
        if !dest_is_dir || !self.dest_existed {
            targets.push(dest_root.to_path_buf());
        }
        targets.extend(entry_paths.iter().map(|rel| {
            if dest_is_dir {
                dest_root.join(rel)
            } else {
                dest_root.to_path_buf()
            }
        }));

        // Second line of defence behind sanitize_entry_path: nothing outside the
        // destination gets chowned, whatever the archive claimed its names were.
        targets.retain(|target| {
            (target.starts_with(dest_root) || self.created_dirs.contains(target))
                && !self.existing_dirs.contains(target)
        });
        targets
    }
}

/// Undo what a refused streamed upload wrote.
///
/// The hinted arm extracts before it can check the mounts — the shape hint is
/// what saves the spool, so there is no archive left to pre-scan — which used
/// to leave a refused copy half-applied on the rootfs, invisible beneath the
/// mount that caused the refusal. Removing exactly [`DestBefore::created`],
/// the same set the ownership hand-off claims as "made by this copy", puts the
/// tree back.
///
/// Not a perfect inverse: a file the archive *overwrote* lost its old content
/// the moment extraction wrote it, so it is removed rather than left carrying
/// the refused payload's bytes. Directories the image shipped are never
/// touched — [`DestBefore::created`] excludes them.
///
/// Best-effort per node, and deepest-first so a directory is empty by the time
/// it is removed. A node a workload holds or races simply stays; the refusal is
/// what the caller sees either way.
fn remove_extracted(dest_root: &Path, entry_paths: &[PathBuf], before: &DestBefore) {
    let mut targets = before.created(dest_root, entry_paths);
    targets.sort_by_key(|path| std::cmp::Reverse(path.components().count()));
    for target in targets {
        let removed = if target.is_dir() {
            std::fs::remove_dir(&target)
        } else {
            std::fs::remove_file(&target)
        };
        match removed {
            Ok(()) => {}
            // Already gone, or a directory still holding paths this copy did
            // not create — both mean there is nothing here to undo.
            Err(e)
                if e.kind() == std::io::ErrorKind::NotFound
                    || e.kind() == std::io::ErrorKind::DirectoryNotEmpty => {}
            Err(e) => warn!(
                path = %target.display(),
                error = %e,
                "could not roll back a refused upload's payload"
            ),
        }
    }
}

// ── Refusal wording ───────────────────────────────────────────────
//
// A copy meets a mount in three places — the path the request names, a path an
// archive entry would land on, and a mount sitting inside a directory being read
// out — and each says so differently. The phrasing is a shipped contract, not
// decoration: `examples/python/02_features/copy_files.py` and
// `examples/node/cp_tmpfs_workaround.js` both *fail closed* on the
// `'<mount>' mount` fragment — matching a bare `/tmp` would also match the
// runtime's own staging tar path — so a rewording that drops it turns both demos
// into hard errors with nothing else going red. The three live together so a
// rewording of one is read next to the others, and
// [`tests::every_mount_refusal_names_the_mount_the_way_the_examples_match`] pins
// the fragment in each.

/// Why the path the request names cannot be transferred, and what to do instead.
fn unreachable_mount_message(in_container: &Path, mount: &Path) -> String {
    format!(
        "{} is under the container's '{}' mount, which file transfer cannot reach; \
         copy to a path outside '{}' (for example /workspace), or pipe a tar through \
         exec: exec([\"tar\", \"xf\", \"-\", \"-C\", \"{}\"])",
        in_container.display(),
        mount.display(),
        mount.display(),
        mount.display(),
    )
}

/// Why an archive entry cannot be written, when the destination root itself was
/// reachable but the payload would land under a mount.
fn unreachable_payload_message(landed: &Path, mount: &Path) -> String {
    format!(
        "this copy would write {} under the container's '{}' mount, which file \
         transfer cannot reach; copy to a path outside '{}', or pipe a tar through \
         exec: exec([\"tar\", \"xf\", \"-\", \"-C\", \"{}\"])",
        landed.display(),
        mount.display(),
        mount.display(),
        mount.display(),
    )
}

/// Why a directory cannot be read out, when a mount sits somewhere inside it.
fn unreadable_subtree_message(src_in_container: &Path, mount: &Path) -> String {
    format!(
        "{} contains the container's '{}' mount, which file transfer cannot read; \
         copying it would return the image's file rather than the one the box sees. \
         Copy a path that excludes '{}', or pipe a tar through exec: \
         exec([\"tar\", \"cf\", \"-\", \"-C\", \"{}\", \".\"])",
        src_in_container.display(),
        mount.display(),
        mount.display(),
        src_in_container.display(),
    )
}

/// Normalize a request path to how the container sees it: absolute, no `..`.
///
/// Mount destinations in the OCI spec are absolute, so comparisons only line up
/// once the request is expressed the same way.
#[allow(clippy::result_large_err)]
fn to_container_path(path: &str) -> Result<PathBuf, Status> {
    let path_obj = Path::new(path);
    if path_obj
        .components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err(Status::invalid_argument("path must not contain .."));
    }
    Ok(Path::new("/").join(path_obj.strip_prefix("/").unwrap_or(path_obj)))
}

/// The deepest mount destination that covers `path` (at or below it), if any.
///
/// Deepest so a message names `/dev/shm` rather than `/dev`.
fn deepest_covering(path: &Path, mounts: &[PathBuf]) -> Option<PathBuf> {
    mounts
        .iter()
        .filter(|mount| path.starts_with(mount))
        .max_by_key(|mount| mount.components().count())
        .cloned()
}

/// One request path resolved against one container.
///
/// Copying is only ever safe where the rootfs layer and the container's own
/// mount namespace agree, so every question a transfer asks — may I write
/// here, may I read this subtree, will this payload land somewhere the box
/// cannot see — is a question about the same pair: the path, and the mounts
/// that might hide it. Holding both together is what lets this answer them.
///
/// Resolved once per request. Previously each check re-derived the container
/// path and re-fetched the mount list behind two locks, and every call site
/// had to destructure a different tuple and pick the matching message; a
/// refusal now returns ready to `?`.
struct CopyTarget {
    /// The path as the container sees it: absolute, no `..`.
    in_container: PathBuf,
    /// Where that path lands on the guest-side rootfs directory.
    on_rootfs: PathBuf,
    /// Destinations of every mount the container was created with.
    mounts: Vec<PathBuf>,
}

impl CopyTarget {
    /// Resolve `path` for `container_id`, refusing it outright when a mount
    /// hides the path itself.
    ///
    /// File transfer works on the rootfs layer from outside the container's
    /// mount namespace. The container mounts a tmpfs over `/tmp` (and binds
    /// volumes elsewhere) *inside* that namespace, so a path at or below one of
    /// those destinations resolves to a different inode than the one any
    /// process in the box sees: writes land under the mount and are invisible
    /// forever, reads come back stale or missing. Refusing is the only honest
    /// answer available from out here — silently transferring the shadow is
    /// what made this a bug rather than a limitation.
    ///
    /// Refusing during construction rather than at each caller is deliberate:
    /// the rootfs path this hands back is only meaningful because it has
    /// already been proven reachable, so there is no way to hold one without
    /// the other.
    #[allow(clippy::result_large_err)]
    async fn resolve(server: &GuestServer, container_id: &str, path: &str) -> Result<Self, Status> {
        let in_container = to_container_path(path)?;
        let mounts = server.container_mounts(container_id).await?;

        if let Some(mount) = deepest_covering(&in_container, &mounts) {
            return Err(Status::failed_precondition(unreachable_mount_message(
                &in_container,
                &mount,
            )));
        }

        let rel = in_container.strip_prefix("/").unwrap_or(&in_container);
        let on_rootfs = server
            .layout
            .shared()
            .container(container_id)
            .rootfs_dir()
            .join(rel);

        Ok(Self {
            in_container,
            on_rootfs,
            mounts,
        })
    }

    /// Where to read or write on the guest-side rootfs.
    fn on_rootfs(&self) -> &Path {
        &self.on_rootfs
    }

    /// Refuse an upload whose payload would land under a mount.
    ///
    /// The root can be perfectly reachable while the payload is not:
    /// `copy_in(dir, "/etc")` where `dir` contains `hosts` would write the
    /// image's `/etc/hosts` beneath the bind, invisible to the box. Checked
    /// against the archive's own entries so a single file to `/etc` — which
    /// lands at `/etc/motd.txt`, under no mount — still works.
    #[allow(clippy::result_large_err)]
    fn refuse_shadowed_payload(&self, entry_paths: &[PathBuf]) -> Result<(), Status> {
        for rel in entry_paths {
            let landed = self.in_container.join(rel);
            if let Some(mount) = deepest_covering(&landed, &self.mounts) {
                return Err(Status::failed_precondition(unreachable_payload_message(
                    &landed, &mount,
                )));
            }
        }
        Ok(())
    }

    /// Refuse a read-out whose subtree contains a mount.
    ///
    /// Packing walks the rootfs layer, so a mount anywhere inside the source
    /// tree would be read through rather than seen — the archive would carry
    /// the image's file where the workload has the mount's. Same bug as the
    /// write direction, one level down, so it is refused the same way.
    #[allow(clippy::result_large_err)]
    fn refuse_shadowed_subtree(&self) -> Result<(), Status> {
        let deepest_below = self
            .mounts
            .iter()
            .filter(|mount| {
                mount.as_path() != self.in_container && mount.starts_with(&self.in_container)
            })
            .max_by_key(|mount| mount.components().count());

        match deepest_below {
            Some(mount) => Err(Status::failed_precondition(unreadable_subtree_message(
                &self.in_container,
                mount,
            ))),
            None => Ok(()),
        }
    }
}

#[tonic::async_trait]
impl Files for GuestServer {
    async fn upload(
        &self,
        request: Request<Streaming<UploadChunk>>,
    ) -> Result<Response<UploadResponse>, Status> {
        let mut stream = request.into_inner();

        // First chunk must carry dest_path (and optional container_id)
        let first = stream
            .message()
            .await?
            .ok_or_else(|| Status::invalid_argument("empty upload stream"))?;

        let dest_path = first.dest_path.clone();
        if dest_path.is_empty() {
            return Err(Status::invalid_argument(
                "dest_path is required in first chunk",
            ));
        }
        let container_id = self
            .resolve_container_id(first.container_id.as_str())
            .await
            .map_err(Status::failed_precondition)?;

        // Resolves the destination and refuses it if a mount hides it.
        let dest = CopyTarget::resolve(self, &container_id, &dest_path).await?;
        let dest_root = dest.on_rootfs().to_path_buf();

        // Overwrite / mkdir flags
        let mkdir_parents = first.mkdir_parents;
        let overwrite = first.overwrite;
        let source_is_dir = first.source_is_dir;
        let first_data = first.data;

        match source_is_dir {
            // Streaming path: unpack directly from the byte stream. The hint
            // carries the *source* shape, but the destination can still force
            // directory mode — same as `detect_extraction_mode` in the legacy
            // path. Fold the destination-side signals back in here so
            // "single file → existing directory" keeps landing inside it
            // (Unix cp semantics), not overwriting the directory path.
            Some(source_is_dir) => {
                let force_directory =
                    source_is_dir || dest_path.ends_with('/') || dest_root.is_dir();
                let tar_stream: BoxByteStream = Box::pin(async_stream::stream! {
                    if !first_data.is_empty() {
                        yield Ok(first_data);
                    }
                    loop {
                        match stream.message().await {
                            Ok(Some(chunk)) => {
                                if !chunk.data.is_empty() {
                                    yield Ok(chunk.data);
                                }
                            }
                            Ok(None) => break,
                            Err(e) => {
                                yield Err(std::io::Error::other(e));
                                break;
                            }
                        }
                    }
                });

                // The two pre-extraction facts the streamed path can still
                // sample — whether the destination existed, and which
                // ancestors mkdir_parents still has to conjure. Both must be
                // captured BEFORE extraction: afterwards the ancestors exist
                // and are indistinguishable from ones the image shipped.
                let dest_existed = dest_root.exists();
                let created_dirs = missing_ancestors(&dest_root);
                let report = boxlite_shared::tar::unpack_stream(
                    tar_stream,
                    dest_root.clone(),
                    boxlite_shared::tar::UnpackContext {
                        overwrite,
                        mkdir_parents,
                        force_directory,
                    },
                )
                .await
                .map_err(|e| Status::internal(e.to_string()))?;
                let boxlite_shared::tar::UnpackReport {
                    entry_paths,
                    preexisting_dirs,
                } = report;
                let existing_dirs = preexisting_dirs
                    .into_iter()
                    .map(|rel| dest_root.join(rel))
                    .collect();
                let entry_paths: Arc<[PathBuf]> = entry_paths.into();
                let before = DestBefore::recorded(created_dirs, dest_existed, existing_dirs);

                // The stream cannot be pre-scanned without spooling, so the
                // mount-shadow refusal runs *after* extraction — the hintless
                // arm refuses before writing a byte. The payload is therefore
                // rolled back here, so a refused copy leaves nothing behind
                // either way.
                if let Err(refusal) = dest.refuse_shadowed_payload(&entry_paths) {
                    remove_extracted(&dest_root, &entry_paths, &before);
                    return Err(refusal);
                }

                // Hand the recorded paths to the box user, plus the
                // ancestors captured above — the recording names what the
                // archive made, the pre-sampled ancestors name what
                // mkdir_parents made.
                self.chown_to_container_user(
                    &container_id,
                    dest_root.clone(),
                    Arc::clone(&entry_paths),
                    before,
                )
                .await;
            }
            // Legacy fallback for older hosts/runners that omit the hint:
            // buffer to a size-capped temp file and detect the shape by peeking.
            None => {
                // Temp file to hold the tar stream. `StagedTar` owns the
                // deletion, because every step from here to the end can leave
                // through a `?`. Created only here — the hinted arm streams
                // straight into extraction and never stages.
                let staged = StagedTar::new(
                    std::env::temp_dir()
                        .join(format!("boxlite-upload-{}.tar", uuid::Uuid::new_v4())),
                );
                let mut file = File::create(staged.path())
                    .await
                    .map_err(|e| Status::internal(format!("failed to create temp file: {}", e)))?;

                let mut total: u64 = 0;
                if !first_data.is_empty() {
                    total += first_data.len() as u64;
                    if total > MAX_UPLOAD_BYTES {
                        return Err(Status::resource_exhausted("upload too large"));
                    }
                    file.write_all(&first_data).await.map_err(|e| {
                        Status::internal(format!("failed to write temp file: {}", e))
                    })?;
                }

                while let Some(chunk) = stream.message().await? {
                    let len = chunk.data.len() as u64;
                    total += len;
                    if total > MAX_UPLOAD_BYTES {
                        return Err(Status::resource_exhausted("upload too large"));
                    }
                    file.write_all(&chunk.data).await.map_err(|e| {
                        Status::internal(format!("failed to write temp file: {}", e))
                    })?;
                }

                file.flush()
                    .await
                    .map_err(|e| Status::internal(format!("failed to flush temp file: {}", e)))?;

                // Names the archive carries, read once: the mount check below
                // and the ownership hand-off afterwards must agree on what
                // this copy wrote. Shared rather than cloned — an archive may
                // carry a great many names.
                let entry_paths: Arc<[PathBuf]> =
                    boxlite_shared::tar::entry_paths(staged.path().to_path_buf())
                        .await
                        .map_err(|e| Status::internal(e.to_string()))?
                        .into();

                // The root cleared the mount check, but individual entries may
                // still land under one. Refuse before touching the rootfs — a
                // partially applied copy is worse than a refused one.
                dest.refuse_shadowed_payload(&entry_paths)?;

                // What is not there yet — unpack is about to create it, and it
                // needs the same owner as the payload. Must be sampled *before*
                // extraction, since afterwards there is no way to tell what we
                // made from what the image already shipped.
                let before =
                    DestBefore::sample(dest_root.clone(), Arc::clone(&entry_paths)).await?;

                // dest_path may have a trailing '/' indicating directory mode.
                let force_directory = dest_path.ends_with('/');
                boxlite_shared::tar::unpack(
                    staged.path().to_path_buf(),
                    dest_root.clone(),
                    boxlite_shared::tar::UnpackContext {
                        overwrite,
                        mkdir_parents,
                        force_directory,
                    },
                )
                .await
                .map_err(|e| Status::internal(e.to_string()))?;

                // Hand the payload to the user the box actually runs as. tar
                // preserves neither owner (it extracts as this process, root)
                // nor any notion of who will read the file, so without this
                // every copy_in lands root-owned and a non-root workload
                // cannot open it.
                self.chown_to_container_user(&container_id, dest_root.clone(), entry_paths, before)
                    .await;
            }
        }

        info!(
            dest = %dest_root.display(),
            container_id = %container_id,
            "upload completed"
        );

        Ok(Response::new(UploadResponse {
            success: true,
            error: None,
        }))
    }

    type DownloadStream = ReceiverStream<Result<DownloadChunk, Status>>;

    async fn download(
        &self,
        request: Request<DownloadRequest>,
    ) -> Result<Response<Self::DownloadStream>, Status> {
        let req = request.into_inner();
        if req.src_path.is_empty() {
            return Err(Status::invalid_argument("src_path is required"));
        }
        let container_id = self
            .resolve_container_id(req.container_id.as_str())
            .await
            .map_err(Status::failed_precondition)?;

        let src = CopyTarget::resolve(self, &container_id, &req.src_path).await?;
        let src_path = src.on_rootfs().to_path_buf();
        if !src_path.exists() {
            return Err(Status::not_found("source path does not exist"));
        }

        if src_path.is_dir() {
            src.refuse_shadowed_subtree()?;
        }

        let include_parent = req.include_parent;
        let follow_symlinks = req.follow_symlinks;

        // Stream the tar directly from disk (no temp file); the first chunk
        // carries the source_is_dir shape hint.
        let (source_is_dir, tar_stream) = boxlite_shared::tar::pack_stream(
            src_path,
            boxlite_shared::tar::PackContext {
                follow_symlinks,
                include_parent,
            },
        )
        .await
        .map_err(|e| Status::internal(e.to_string()))?;

        let (tx, rx) = mpsc::channel::<Result<DownloadChunk, Status>>(4);
        tokio::spawn(async move {
            let mut first = true;
            let mut stream = tar_stream;
            while let Some(item) = stream.next().await {
                match item {
                    Ok(data) => {
                        let chunk = DownloadChunk {
                            data,
                            source_is_dir: if first { Some(source_is_dir) } else { None },
                        };
                        first = false;
                        if tx.send(Ok(chunk)).await.is_err() {
                            break;
                        }
                    }
                    Err(e) => {
                        let _ = tx.send(Err(Status::internal(e.to_string()))).await;
                        break;
                    }
                }
            }
        });

        info!(
            src = %req.src_path,
            container_id = %container_id,
            "download started"
        );

        Ok(Response::new(ReceiverStream::new(rx)))
    }
}

impl GuestServer {
    async fn resolve_container_id(&self, requested: &str) -> Result<String, String> {
        if !requested.is_empty() {
            return Ok(requested.to_string());
        }

        let containers = self.containers.lock().await;
        if containers.len() == 1 {
            if let Some((id, _)) = containers.iter().next() {
                return Ok(id.clone());
            }
        }
        Err("container_id required when multiple containers present".into())
    }

    /// Give everything this upload created to the container's own user.
    ///
    /// [`DestBefore::created`] decides the scope; this only carries it out.
    ///
    /// Best-effort throughout: the bytes are already on disk by the time this
    /// runs, so a failure here must not be reported as the copy failing.
    async fn chown_to_container_user(
        &self,
        container_id: &str,
        dest_root: PathBuf,
        entry_paths: Arc<[PathBuf]>,
        before: DestBefore,
    ) {
        let container_arc = {
            let containers = self.containers.lock().await;
            containers.get(container_id).cloned()
        };
        let Some(container_arc) = container_arc else {
            tracing::warn!(
                container_id = %container_id,
                "container vanished before copied files could be given to its user"
            );
            return;
        };
        let (uid, gid) = {
            let container = container_arc.lock().await;
            container.user()
        };
        if (uid, gid) == (0, 0) {
            return;
        }
        let rootfs_root = self
            .layout
            .shared()
            .container(container_id)
            .rootfs_dir()
            .to_path_buf();

        // Scoping the targets and changing them are both one syscall per
        // archive entry, so this runs on a blocking thread for the same reason
        // the sampling did.
        let handoff = tokio::task::spawn_blocking(move || {
            let targets = before.created(&dest_root, &entry_paths);
            let (mut changed, mut failed) = (0usize, 0usize);

            // Anchored at the rootfs rather than at `dest_root`: the parents
            // `mkdir_parents` conjured sit above the destination, and the
            // rootfs is the boundary none of them may cross.
            let anchor = match std::fs::File::open(&rootfs_root) {
                Ok(dir) => dir,
                Err(e) => {
                    tracing::warn!(
                        rootfs = %rootfs_root.display(), error = %e,
                        "copied paths could not be given to the container user"
                    );
                    return (0, targets.len());
                }
            };

            for target in &targets {
                match chown_within_rootfs(anchor.as_raw_fd(), &rootfs_root, target, uid, gid) {
                    Ok(()) => changed += 1,
                    Err(_) => failed += 1,
                }
            }
            (changed, failed)
        })
        .await;

        let (changed, failed) = match handoff {
            Ok(counts) => counts,
            Err(e) => {
                tracing::warn!(
                    container_id = %container_id,
                    uid, gid, error = %e,
                    "copied paths could not be given to the container user"
                );
                return;
            }
        };
        if failed > 0 {
            tracing::warn!(
                container_id = %container_id,
                uid, gid, changed, failed,
                "some copied paths could not be given to the container user"
            );
        }
    }

    /// Mount destinations of the container, from the OCI spec it was created
    /// with.
    ///
    /// Asked once per copy, by [`CopyTarget::resolve`], which then answers
    /// every later question from the list it holds. The answer costs only the
    /// clone: `Container` resolved the list at creation, so nothing here reads
    /// a file while holding the container's lock on the guest's async runtime.
    #[allow(clippy::result_large_err)]
    async fn container_mounts(&self, container_id: &str) -> Result<Vec<PathBuf>, Status> {
        let container_arc = {
            let containers = self.containers.lock().await;
            containers.get(container_id).cloned().ok_or_else(|| {
                Status::failed_precondition(format!("container not found: {container_id}"))
            })?
        };
        let container = container_arc.lock().await;
        Ok(container.mount_destinations().to_vec())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `/etc` holds the `/etc/hosts` bind but is not itself a mount, so naming
    /// it is fine — only paths at or below a mount are unreachable.
    #[test]
    fn only_paths_at_or_below_a_mount_are_covered() {
        let mounts = [PathBuf::from("/tmp"), PathBuf::from("/etc/hosts")];

        assert_eq!(
            deepest_covering(Path::new("/tmp/x"), &mounts),
            Some(PathBuf::from("/tmp"))
        );
        assert_eq!(deepest_covering(Path::new("/etc"), &mounts), None);
        assert_eq!(deepest_covering(Path::new("/tmpfoo"), &mounts), None);
    }

    /// Deepest wins so the message names the mount that actually blocks.
    #[test]
    fn the_deepest_covering_mount_is_reported() {
        let mounts = [PathBuf::from("/dev"), PathBuf::from("/dev/shm")];

        assert_eq!(
            deepest_covering(Path::new("/dev/shm/x"), &mounts),
            Some(PathBuf::from("/dev/shm"))
        );
    }

    /// A `CopyTarget` for `in_container`, with no rootfs behind it — enough to
    /// exercise the reachability checks, which read only the path and the mounts.
    fn target(in_container: &str, mounts: &[&str]) -> CopyTarget {
        CopyTarget {
            in_container: PathBuf::from(in_container),
            on_rootfs: PathBuf::from("/unused"),
            mounts: mounts.iter().map(PathBuf::from).collect(),
        }
    }

    /// A reachable destination does not make its payload reachable:
    /// `copy_in(dir, "/etc")` carrying `hosts` lands on the image's shadowed
    /// `/etc/hosts`, not the bind the workload reads.
    #[test]
    fn an_entry_landing_under_a_mount_is_refused_though_the_root_was_fine() {
        let etc = target("/etc", &["/etc/hosts", "/tmp"]);

        // The root itself is clear — `/etc` merely contains a mount.
        assert!(deepest_covering(Path::new("/etc"), &etc.mounts).is_none());

        let err = etc
            .refuse_shadowed_payload(&[PathBuf::from("motd.txt"), PathBuf::from("hosts")])
            .expect_err("an entry landing on the bind must be refused");
        assert!(err.message().contains("'/etc/hosts' mount"), "{err:?}");

        etc.refuse_shadowed_payload(&[PathBuf::from("motd.txt")])
            .expect("an entry under no mount still copies");
    }

    /// Reading out a directory that *contains* a mount would tar the shadowed
    /// image file. A path that IS a mount is the other check's job, so it must
    /// not also trip this one — otherwise the two would report the same refusal
    /// twice with different wording.
    #[test]
    fn a_directory_holding_a_mount_cannot_be_read_out() {
        let err = target("/etc", &["/etc/hosts"])
            .refuse_shadowed_subtree()
            .expect_err("a mount inside the source tree must be refused");
        assert!(err.message().contains("'/etc/hosts' mount"), "{err:?}");

        target("/tmp", &["/tmp"])
            .refuse_shadowed_subtree()
            .expect("a path that is itself a mount is not a mount *below* it");

        target("/workspace", &["/tmp"])
            .refuse_shadowed_subtree()
            .expect("an unrelated mount does not block the read");
    }

    /// The refusal wording is a shipped contract, not prose. Both copy examples
    /// fail closed on the `'<mount>' mount` fragment (a bare `/tmp` would also
    /// match the runtime's own staging tar path), so a rewording that drops it
    /// breaks them with nothing else going red.
    ///
    /// All three refusals, not just the one the examples happen to trigger: a
    /// caller taught to match that fragment meets whichever of the three its
    /// copy runs into, and the payload and subtree wordings are the ones no
    /// test had ever read.
    #[test]
    fn every_mount_refusal_names_the_mount_the_way_the_examples_match() {
        let refusals = [
            (
                "request path",
                unreachable_mount_message(Path::new("/tmp/ghost.txt"), Path::new("/tmp")),
            ),
            (
                "archive entry",
                unreachable_payload_message(Path::new("/tmp/ghost.txt"), Path::new("/tmp")),
            ),
            (
                "subtree read out",
                unreadable_subtree_message(Path::new("/"), Path::new("/tmp")),
            ),
        ];

        for (site, message) in refusals {
            assert!(
                message.contains("'/tmp' mount"),
                "the {site} refusal drops the fragment \
                 examples/python/02_features/copy_files.py and \
                 examples/node/cp_tmpfs_workaround.js match exactly: {message}"
            );
        }
    }

    /// Nothing the image shipped changes owner, however the copy reaches it.
    ///
    /// A destination directory is the shape the *request* names; an entry
    /// directory is the shape the *archive* names, which
    /// `copy_in("./dist", "/usr/local", include_parent=false)` produces. Only
    /// the second was unguarded: `/usr/local/bin` is the image's, `bin/tool` is
    /// ours.
    #[test]
    fn directories_the_image_shipped_keep_their_owner() {
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("local");
        std::fs::create_dir_all(dest.join("bin")).unwrap();
        let entries = vec![PathBuf::from("bin"), PathBuf::from("bin/tool")];

        let before = DestBefore::sample_blocking(&dest, &entries);
        std::fs::write(dest.join("bin/tool"), b"payload").unwrap();

        assert_eq!(
            before.created(&dest, &entries),
            vec![dest.join("bin/tool")],
            "only the file this copy wrote is ours to hand over"
        );
    }

    /// The other half: what the copy did conjure is all in scope — the
    /// destination it made, the parents `mkdir_parents` made for it, and the
    /// directories the archive named that were not there before.
    #[test]
    fn everything_the_copy_conjured_is_handed_over() {
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("srv/probe");
        let entries = vec![PathBuf::from("pkg"), PathBuf::from("pkg/file.txt")];

        let before = DestBefore::sample_blocking(&dest, &entries);
        std::fs::create_dir_all(dest.join("pkg")).unwrap();
        std::fs::write(dest.join("pkg/file.txt"), b"payload").unwrap();

        assert_eq!(
            before.created(&dest, &entries),
            vec![
                tmp.path().join("srv"),
                dest.clone(),
                dest.join("pkg"),
                dest.join("pkg/file.txt"),
            ]
        );
    }

    /// A destination *file* is ours whether or not it was there: extraction
    /// just overwrote it, and single-file mode lands every entry on it.
    #[test]
    fn an_existing_destination_file_is_still_ours() {
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("payload.txt");
        std::fs::write(&dest, b"old").unwrap();
        let entries = vec![PathBuf::from("payload.txt")];

        let before = DestBefore::sample_blocking(&dest, &entries);
        std::fs::write(&dest, b"new").unwrap();

        let created = before.created(&dest, &entries);
        assert!(!created.is_empty());
        assert!(created.iter().all(|target| target == &dest), "{created:?}");
    }

    /// A symlinked directory the *image* shipped must still get the hand-off.
    ///
    /// `python:3-slim` ships `/lib -> usr/lib`, `redis:7.4-alpine` ships
    /// `/var/run -> ../run`. Neither is a mount, so a copy to `/lib/x` is
    /// reachable and extraction writes it — through the link, into `usr/lib`.
    /// Refusing to follow that link would leave the file root-owned, which is
    /// the ownership bug this hand-off exists to fix, so confining the
    /// resolution has to mean following it, not rejecting it.
    #[test]
    fn a_symlink_ancestor_inside_the_rootfs_still_gets_the_hand_off() {
        let tmp = tempfile::tempdir().unwrap();
        let rootfs = tmp.path().join("rootfs");
        // Both shapes real images ship. The absolute one is the load-bearing
        // case: `RESOLVE_BENEATH` refuses it as an escape, so it is the only
        // form that fails if the resolver is ever "hardened" to BENEATH — the
        // relative one resolves under either flag and would not notice.
        std::fs::create_dir_all(rootfs.join("usr/lib")).unwrap();
        std::os::unix::fs::symlink("usr/lib", rootfs.join("lib")).unwrap();
        std::fs::write(rootfs.join("usr/lib/payload"), b"x").unwrap();

        std::fs::create_dir_all(rootfs.join("run")).unwrap();
        std::fs::create_dir_all(rootfs.join("var")).unwrap();
        std::os::unix::fs::symlink("/run", rootfs.join("var/run")).unwrap();
        std::fs::write(rootfs.join("run/payload"), b"x").unwrap();

        let anchor = std::fs::File::open(&rootfs).unwrap();
        let (uid, gid) = (
            nix::unistd::Uid::current().as_raw(),
            nix::unistd::Gid::current().as_raw(),
        );

        for through in ["lib/payload", "var/run/payload"] {
            chown_within_rootfs(anchor.as_raw_fd(), &rootfs, &rootfs.join(through), uid, gid)
                .unwrap_or_else(|e| {
                    panic!("a copy through an image-shipped symlink ({through}) must still be handed over, got {e:?}")
                });
        }
    }

    /// The hand-off must not reach a file outside the rootfs through a
    /// directory the workload swapped for a symlink after extraction.
    ///
    /// The discriminating assertion is the refusal itself: with the fix reverted
    /// to `fchownat(None, target, …)` this call returns `Ok`, because
    /// `AT_SYMLINK_NOFOLLOW` guards only the leaf. An unprivileged test cannot
    /// observe a cross-uid change, so proving *where* it landed is not available
    /// here — that the escape is refused at all is.
    #[test]
    fn the_hand_off_does_not_chown_through_a_swapped_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let rootfs = tmp.path().join("rootfs");
        let elsewhere = tmp.path().join("elsewhere");
        std::fs::create_dir_all(rootfs.join("app")).unwrap();
        std::fs::create_dir_all(&elsewhere).unwrap();
        std::fs::write(elsewhere.join("passwd"), b"outside").unwrap();

        let target = rootfs.join("app/passwd");
        std::fs::write(&target, b"payload").unwrap();

        let anchor = std::fs::File::open(&rootfs).unwrap();
        let (uid, gid) = (
            nix::unistd::Uid::current().as_raw(),
            nix::unistd::Gid::current().as_raw(),
        );

        // The path extraction actually produced is handed over.
        chown_within_rootfs(anchor.as_raw_fd(), &rootfs, &target, uid, gid)
            .expect("a path this copy created is ours to hand over");

        // The workload swaps the directory for a link out of the rootfs. The
        // absolute target is reinterpreted against the rootfs, so the same
        // request can no longer name the outside file.
        std::fs::remove_file(&target).unwrap();
        std::fs::remove_dir(rootfs.join("app")).unwrap();
        std::os::unix::fs::symlink(&elsewhere, rootfs.join("app")).unwrap();

        chown_within_rootfs(anchor.as_raw_fd(), &rootfs, &target, uid, gid)
            .expect_err("the hand-off must not reach through a swapped directory");
    }

    /// A target the lexical filter should already have dropped is refused here
    /// too, rather than chowned on a lexical say-so.
    #[test]
    fn a_target_outside_the_rootfs_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let rootfs = tmp.path().join("rootfs");
        std::fs::create_dir_all(&rootfs).unwrap();
        let outside = tmp.path().join("outside.txt");
        std::fs::write(&outside, b"x").unwrap();

        let anchor = std::fs::File::open(&rootfs).unwrap();
        let err = chown_within_rootfs(anchor.as_raw_fd(), &rootfs, &outside, 0, 0)
            .expect_err("a target outside the rootfs must be refused");
        assert_eq!(err, nix::errno::Errno::EXDEV, "{err:?}");
    }

    #[test]
    fn container_paths_are_absolute_and_reject_parent_dirs() {
        assert_eq!(
            to_container_path("/tmp/x").unwrap(),
            PathBuf::from("/tmp/x")
        );
        assert_eq!(to_container_path("tmp/x").unwrap(), PathBuf::from("/tmp/x"));
        assert!(to_container_path("/a/../b").is_err());
    }
}

/// The spool tar must be removed on drop — covering every error path of
/// the hintless upload arm, not just the success path. The guest agent's
/// /tmp is not visible to container processes, so this is asserted here
/// rather than from the copy integration tests.
#[test]
fn staged_tar_removes_file_on_drop() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path: std::path::PathBuf = dir.path().join("spool.tar");
    std::fs::write(&path, b"partial tar").unwrap();

    {
        let _guard = StagedTar::new(path.clone());
        assert!(path.exists(), "guard must not remove the file while alive");
    }

    assert!(!path.exists(), "guard must remove the file on drop");
}

/// Dropping a guard whose file was already removed (or never created) is
/// harmless — the error-path cleanup must not panic.
#[test]
fn staged_tar_tolerates_missing_file() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path: std::path::PathBuf = dir.path().join("never-created.tar");

    let _guard = StagedTar::new(path);
    // Dropping happens at end of scope; no panic is the assertion.
}

/// A refused streamed upload must undo its own writes — the payload it landed
/// beneath the mount and the directories it conjured on the way there.
#[test]
fn rollback_removes_what_the_copy_wrote() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let dest_root = tmp.path().join("dest");
    // What the archive carried: a file at the root and one a level down.
    std::fs::create_dir_all(dest_root.join("nested")).unwrap();
    std::fs::write(dest_root.join("landed.txt"), b"payload").unwrap();
    std::fs::write(dest_root.join("nested/deep.txt"), b"payload").unwrap();
    let entries = [
        PathBuf::from("landed.txt"),
        PathBuf::from("nested"),
        PathBuf::from("nested/deep.txt"),
    ];

    // The destination and both directories are ours: nothing pre-existed.
    let before = DestBefore::recorded(vec![dest_root.clone()], false, HashSet::new());
    remove_extracted(&dest_root, &entries, &before);

    assert!(
        !dest_root.exists(),
        "a destination this copy created must go with it"
    );
}

/// The inverse must stop at the tree's edge: a directory the image shipped is
/// not this copy's to delete, even when the archive wrote into it.
#[test]
fn rollback_keeps_directories_it_did_not_create() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let dest_root = tmp.path().join("usr-local");
    let shipped = dest_root.join("bin");
    std::fs::create_dir_all(&shipped).unwrap();
    std::fs::write(shipped.join("preexisting"), b"from the image").unwrap();
    std::fs::write(shipped.join("tool"), b"payload").unwrap();
    let entries = [PathBuf::from("bin"), PathBuf::from("bin/tool")];

    // `dest_root` and `bin` were both already there — the recorded shape a
    // streamed unpack reports when it extracts into an existing tree.
    let before = DestBefore::recorded(
        Vec::new(),
        true,
        HashSet::from([dest_root.clone(), shipped.clone()]),
    );
    remove_extracted(&dest_root, &entries, &before);

    assert!(
        shipped.is_dir(),
        "an image directory must survive a rollback"
    );
    assert!(
        shipped.join("preexisting").exists(),
        "a file this copy never named must survive a rollback"
    );
    assert!(
        !shipped.join("tool").exists(),
        "the payload the copy wrote must be gone"
    );
}
