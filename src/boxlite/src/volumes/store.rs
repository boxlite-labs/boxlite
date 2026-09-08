//! Named-volume store and metadata type.
//!
//! [`VolumeInfo`] is the storage-agnostic view of a volume returned by the
//! [`VolumeBackend`](crate::runtime::volumes::VolumeBackend) trait and rendered
//! by the CLI. [`LocalNamedVolumeStore`] is the concrete local backend wired into
//! `impl VolumeBackend for LocalRuntime`.
//!
//! On-disk shape under `{home}/volumes/`:
//!
//! ```text
//! {id}/                 one volume
//! {id}/.metadata.json   its name and creation time — host-only
//! {id}/_data/           its payload — the only part a box ever sees
//! ```
//!
//! A volume is one directory: creating it is a `mkdir` and one file, removing
//! it is one `remove_dir_all`, and no sidecar can outlive its payload or be
//! left behind by a crash between two deletes. The payload sits one level
//! down, as in docker's local driver (`volume/local/local.go:31,85`), because
//! the directory handed to a box is shared wholesale: a sidecar inside it
//! could be rewritten by the box to claim another volume's name. A volume
//! whose sidecar is missing anyway answers to its id alone (see `volume_info`).

use std::fs;
use std::io;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};

use crate::runtime::id::{VolumeID, VolumeIDMint};
use boxlite_shared::errors::{BoxliteError, BoxliteResult};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Sidecar file inside a volume's directory — beside the payload, never in it.
const METADATA_FILE: &str = ".metadata.json";

/// Payload directory inside a volume's directory; this, not the volume
/// directory, is what a box mounts.
const PAYLOAD_DIR: &str = "_data";

/// Public metadata about a volume.
///
/// Mirrors the shape of [`crate::runtime::types::ImageInfo`]: a storage-agnostic
/// view suitable for CLI/table rendering.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VolumeInfo {
    /// Server-assigned volume id — the addressing key for get/remove.
    pub id: String,

    /// Volume name, unique within the owning scope. Mountable in place of the
    /// id, so a box can name the volume it wants without knowing the id. The
    /// server defaults it to the id when the caller supplies none.
    pub name: String,

    /// When the volume was created.
    pub created_at: DateTime<Utc>,

    /// Size of the payload in bytes, if it could be computed.
    pub size_bytes: Option<u64>,
}

/// What the store persists about a volume, inside its directory.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct VolumeMetadata {
    name: String,
    created_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct LocalNamedVolumeStore {
    volumes_dir: PathBuf,
}

impl LocalNamedVolumeStore {
    /// Create a store rooted at `{home_dir}/volumes`.
    pub fn new(home_dir: &Path) -> Self {
        Self {
            volumes_dir: home_dir.join("volumes"),
        }
    }

    /// Create a volume, naming it after its id when the caller supplies none.
    ///
    /// The name is what makes `-v my-data:/data` work without knowing the id,
    /// so it has to be unique: a duplicate would make that reference ambiguous
    /// and silently pick one of two volumes.
    pub fn create(&self, name: Option<&str>) -> BoxliteResult<VolumeInfo> {
        let _lock = self.lock_volumes_dir()?;
        self.create_locked(name)
    }

    /// The body of [`Self::create`]; the caller holds the volumes-directory
    /// lock, so the uniqueness scan and the `mkdir` below are one step.
    fn create_locked(&self, name: Option<&str>) -> BoxliteResult<VolumeInfo> {
        let id = VolumeIDMint::mint().to_string();
        let name = match name {
            Some(name) => {
                validate_volume_name(name)?;
                // `locate`, not `find_by_name`: a name equal to another
                // volume's id would pass a name-only scan yet never resolve
                // to this volume, because `locate` tries the id first.
                if self.locate(name)?.is_some() {
                    return Err(BoxliteError::AlreadyExists(format!(
                        "a volume named {name:?} already exists"
                    )));
                }
                name.to_string()
            }
            None => id.clone(),
        };

        let payload = self.volumes_dir.join(&id).join(PAYLOAD_DIR);
        fs::create_dir_all(&payload).map_err(|e| {
            BoxliteError::Storage(format!(
                "failed to create volume dir {}: {}",
                payload.display(),
                e
            ))
        })?;

        let metadata = VolumeMetadata {
            name,
            created_at: Utc::now(),
        };

        if let Err(write_error) = self.write_metadata(&id, &metadata) {
            // A directory without a sidecar would list as an id-named volume,
            // so take it back out. The write error is the one worth reporting;
            // a cleanup failure only adds to it.
            let dir = self.volumes_dir.join(&id);
            if let Err(cleanup_error) = fs::remove_dir_all(&dir) {
                tracing::warn!(
                    volume = %id,
                    error = %cleanup_error,
                    "failed to remove the directory of a volume whose sidecar could not be written"
                );
            }
            return Err(write_error);
        }

        Ok(VolumeInfo {
            id,
            name: metadata.name,
            created_at: metadata.created_at,
            size_bytes: None,
        })
    }

    /// List every volume.
    pub fn list(&self) -> BoxliteResult<Vec<VolumeInfo>> {
        let entries = match fs::read_dir(&self.volumes_dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => {
                return Err(BoxliteError::Storage(format!(
                    "failed to read volume dir {}: {}",
                    self.volumes_dir.display(),
                    e
                )));
            }
        };

        let mut infos = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|e| {
                BoxliteError::Storage(format!(
                    "failed to read an entry of volume dir {}: {}",
                    self.volumes_dir.display(),
                    e
                ))
            })?;
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let id = entry.file_name().to_string_lossy().into_owned();
            if let Some(volume) = self.volume_info(id, &path)? {
                infos.push(volume);
            }
        }
        Ok(infos)
    }

    /// Get metadata for a single volume, by id or by name.
    ///
    /// Returns `BoxliteError::NotFound` when no such volume exists — unlike
    /// [`Self::payload_dir`], inspecting a name never creates it.
    pub fn get(&self, reference: &str) -> BoxliteResult<VolumeInfo> {
        let Some((id, dir)) = self.locate(reference)? else {
            return Err(not_found(reference));
        };
        // The directory can vanish between `locate` and the stat below when a
        // removal races this call; to the caller that is simply "not found".
        self.volume_info(id, &dir)?
            .ok_or_else(|| not_found(reference))
    }

    /// Remove a volume by id or name. With `force`, a missing volume is a
    /// no-op.
    ///
    /// Held under the volumes-directory lock like `create`: a removal that
    /// interleaves with a first-use mount of the same name must leave one
    /// consistent outcome, not a torn scan.
    pub fn remove(&self, reference: &str, force: bool) -> BoxliteResult<()> {
        let _lock = self.lock_volumes_dir()?;
        let Some((_, dir)) = self.locate(reference)? else {
            if force {
                return Ok(());
            }
            return Err(not_found(reference));
        };

        // The sidecar lives inside `dir`, so this frees the name too.
        fs::remove_dir_all(&dir).map_err(|e| {
            BoxliteError::Storage(format!(
                "failed to remove directory {}: {}",
                dir.display(),
                e,
            ))
        })
    }

    /// The directory a box mounts — the volume's `_data/`, never the volume
    /// directory itself, so the sidecar stays out of the guest's reach.
    ///
    /// Deliberately not a field of [`VolumeInfo`]: an end user names a volume
    /// by id or name, and a path on the server's disk is not something a REST
    /// client can act on — docker does return a `Mountpoint`, so this is a
    /// choice, not an impossibility. The answer is also local-only: a REST
    /// runtime has none to give, which is why this is a method on the local
    /// store rather than on the metadata or the `VolumeBackend` trait.
    ///
    /// A reference the store has never seen becomes a new volume of that name,
    /// so `-v my-data:/data` works on first use as it does with docker. Only
    /// the mount path does this; [`Self::get`] and [`Self::remove`] stay strict.
    pub(crate) fn payload_dir(&self, reference: &str) -> BoxliteResult<PathBuf> {
        let _lock = self.lock_volumes_dir()?;
        let dir = match self.locate(reference)? {
            Some((_, dir)) => dir,
            None => {
                let created = self.create_locked(Some(reference))?;
                self.volumes_dir.join(created.id)
            }
        };
        Ok(dir.join(PAYLOAD_DIR))
    }

    /// Find the directory holding `reference`, if any.
    ///
    /// An id names its directory directly, so it costs one `stat`; a name
    /// lives only in the sidecars, so it costs a scan.
    fn locate(&self, reference: &str) -> BoxliteResult<Option<(String, PathBuf)>> {
        validate_reference(reference)?;
        if VolumeID::is_valid(reference) {
            let dir = self.volumes_dir.join(reference);
            if dir.is_dir() {
                return Ok(Some((reference.to_string(), dir)));
            }
        }
        self.find_by_name(reference)
    }

    /// Scan every volume for the one carrying `name`.
    fn find_by_name(&self, name: &str) -> BoxliteResult<Option<(String, PathBuf)>> {
        for volume in self.list()? {
            if volume.name == name {
                let dir = self.volumes_dir.join(&volume.id);
                return Ok(Some((volume.id, dir)));
            }
        }
        Ok(None)
    }

    /// Hold an exclusive `flock` on the volumes directory while it is mutated
    /// — `create` and `remove`. The uniqueness scan and the `mkdir` that
    /// follows it are two steps; without the lock, two first-use mounts of the
    /// same name both pass the scan and end up as two volumes answering to one
    /// name. Readers (`list`, `get`, `find_by_name`) stay lock-free and instead
    /// tolerate the two transient states a mutation can expose: a directory
    /// that vanishes mid-scan (`volume_info`) and a sidecar being replaced
    /// (`write_metadata`). The directory itself is the lock file, so nothing
    /// extra appears in `list`. Dropping the returned handle releases the lock.
    fn lock_volumes_dir(&self) -> BoxliteResult<fs::File> {
        fs::create_dir_all(&self.volumes_dir).map_err(|e| {
            BoxliteError::Storage(format!(
                "failed to create volume dir {}: {}",
                self.volumes_dir.display(),
                e
            ))
        })?;
        let dir = fs::File::open(&self.volumes_dir).map_err(|e| {
            BoxliteError::Storage(format!(
                "failed to open volume dir {} for locking: {}",
                self.volumes_dir.display(),
                e
            ))
        })?;
        // SAFETY: `dir` is an open descriptor for the whole call; flock(2) has
        // no other preconditions.
        if unsafe { libc::flock(dir.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err(BoxliteError::Storage(format!(
                "failed to lock volume dir {}: {}",
                self.volumes_dir.display(),
                io::Error::last_os_error()
            )));
        }
        Ok(dir)
    }

    fn metadata_path(&self, id: &str) -> PathBuf {
        self.volumes_dir.join(id).join(METADATA_FILE)
    }

    /// Write the sidecar into the volume's directory, which [`Self::create`]
    /// has already made.
    fn write_metadata(&self, id: &str, metadata: &VolumeMetadata) -> BoxliteResult<()> {
        let path = self.metadata_path(id);
        // Written under a staging name and renamed into place: `rename` within
        // one directory is atomic, so a lock-free reader opens either no
        // sidecar or a whole one, never a truncated file. The staging name only
        // keeps the bytes in flight out of any reader's path.
        let staging = path.with_extension("json.tmp");
        let body = serde_json::to_vec_pretty(metadata).map_err(|e| {
            BoxliteError::Storage(format!("failed to encode volume metadata for {id}: {e}"))
        })?;
        fs::write(&staging, body).map_err(|e| {
            BoxliteError::Storage(format!(
                "failed to write volume metadata {}: {}",
                staging.display(),
                e
            ))
        })?;
        fs::rename(&staging, &path).map_err(|e| {
            BoxliteError::Storage(format!(
                "failed to move volume metadata into place {}: {}",
                path.display(),
                e,
            ))
        })
    }

    /// Read a volume's sidecar. `None` when it has none — created before the
    /// sidecar existed, or deleted from inside the box.
    fn read_metadata(&self, id: &str) -> BoxliteResult<Option<VolumeMetadata>> {
        let path = self.metadata_path(id);
        let body = match fs::read(&path) {
            Ok(body) => body,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => {
                return Err(BoxliteError::Storage(format!(
                    "failed to read volume metadata {}: {}",
                    path.display(),
                    e
                )));
            }
        };
        serde_json::from_slice(&body).map(Some).map_err(|e| {
            BoxliteError::Storage(format!(
                "failed to decode volume metadata {}: {}",
                path.display(),
                e
            ))
        })
    }

    /// Read a volume's metadata off its directory and sidecar.
    ///
    /// Without a sidecar the volume answers to its id alone and `created_at`
    /// is the directory's birth time. The mtime cannot stand in for it: it
    /// moves every time a box writes into the volume, so a volume's "creation"
    /// time would march forward with its contents. Filesystems that cannot
    /// report a birth time (`ErrorKind::Unsupported`) leave the mtime as the
    /// only available answer; any other IO error is a real failure and is
    /// reported, never papered over with the current time.
    ///
    /// `None` means the directory vanished between the caller's `read_dir`
    /// and the stat here: a volume being removed concurrently is not an error
    /// of the listing, it simply is no longer part of it.
    fn volume_info(&self, id: String, dir: &Path) -> BoxliteResult<Option<VolumeInfo>> {
        if let Some(meta) = self.read_metadata(&id)? {
            return Ok(Some(VolumeInfo {
                id,
                name: meta.name,
                created_at: meta.created_at,
                size_bytes: None,
            }));
        }

        let metadata = match fs::metadata(dir) {
            Ok(metadata) => metadata,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => {
                return Err(BoxliteError::Storage(format!(
                    "failed to stat volume {}: {}",
                    dir.display(),
                    e
                )));
            }
        };
        let created_at = match metadata.created() {
            Ok(created_at) => created_at,
            Err(e) if e.kind() == io::ErrorKind::Unsupported => {
                metadata.modified().map_err(|e| {
                    BoxliteError::Storage(format!(
                        "failed to read modification time of volume {}: {}",
                        dir.display(),
                        e
                    ))
                })?
            }
            Err(e) => {
                return Err(BoxliteError::Storage(format!(
                    "failed to read creation time of volume {}: {}",
                    dir.display(),
                    e
                )));
            }
        };
        Ok(Some(VolumeInfo {
            name: id.clone(),
            id,
            created_at: DateTime::<Utc>::from(created_at),
            size_bytes: None,
        }))
    }
}

fn not_found(reference: &str) -> BoxliteError {
    BoxliteError::NotFound(format!("volume not found: {reference}"))
}

/// Reject a reference that is neither an id nor a name before it is used to
/// address anything.
///
/// Without this, `remove(reference, force = true)` would answer `Ok` for
/// `../escape`: the lookup finds nothing, and `force` reads "nothing to do".
/// A malformed reference is a caller error whether or not `force` is set, so
/// it has to fail before the not-found path can swallow it.
fn validate_reference(reference: &str) -> BoxliteResult<()> {
    if VolumeID::is_valid(reference) || validate_volume_name(reference).is_ok() {
        return Ok(());
    }
    Err(BoxliteError::InvalidArgument(format!(
        "invalid volume reference {reference:?}: expected a volume id or name"
    )))
}

/// Accept the names the CLI documents: at least two characters of
/// `[a-zA-Z0-9][a-zA-Z0-9_.-]`, the same rule docker's daemon applies
/// (`daemon/names/names.go:6-9`). A name is not a path component here — the
/// directory is always the id — but it is user-facing, so it stays printable
/// and shell-safe.
fn validate_volume_name(name: &str) -> BoxliteResult<()> {
    let invalid = || {
        BoxliteError::InvalidArgument(format!(
            "invalid volume name {name:?}: at least two characters of \
             [a-zA-Z0-9][a-zA-Z0-9_.-]"
        ))
    };
    if name.len() < 2 {
        return Err(invalid());
    }
    let mut bytes = name.bytes();
    let first = bytes.next().ok_or_else(invalid)?;
    if !first.is_ascii_alphanumeric() {
        return Err(invalid());
    }
    if !bytes.all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'.' || b == b'-') {
        return Err(invalid());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_then_get_roundtrips() {
        let tmp = tempfile::tempdir().unwrap();
        let store = LocalNamedVolumeStore::new(tmp.path());
        let created = store.create(None).unwrap();

        let dir = store.payload_dir(&created.id).unwrap();
        assert!(dir.is_dir());
        let volume_dir = tmp.path().join("volumes").join(&created.id);
        assert_eq!(volume_dir.join("_data"), dir);
        assert!(
            volume_dir.join(".metadata.json").is_file(),
            "the sidecar lives in the volume's directory, beside the payload"
        );

        let got = store.get(&created.id).unwrap();
        assert_eq!(created.id, got.id);
        assert_eq!(created.created_at, got.created_at);
    }

    /// The server names a volume after its id when the caller supplies none —
    /// so a mount can always use the id, named or not.
    #[test]
    fn an_unnamed_volume_is_named_after_its_id() {
        let tmp = tempfile::tempdir().unwrap();
        let store = LocalNamedVolumeStore::new(tmp.path());
        let created = store.create(None).unwrap();

        assert_eq!(created.id, created.name);
        assert_eq!(created.id, store.get(&created.id).unwrap().name);
    }

    /// The name is the whole point of `-v my-data:/data`: it has to resolve to
    /// the same volume the id does, and it cannot be ambiguous.
    #[test]
    fn a_named_volume_answers_to_its_name() {
        let tmp = tempfile::tempdir().unwrap();
        let store = LocalNamedVolumeStore::new(tmp.path());
        let created = store.create(Some("my-data")).unwrap();

        let by_name = store.get("my-data").unwrap();
        assert_eq!(created.id, by_name.id);
        assert_eq!("my-data", by_name.name);
        assert_eq!(
            store.payload_dir(&created.id).unwrap(),
            store.payload_dir("my-data").unwrap()
        );

        let duplicate = store.create(Some("my-data")).unwrap_err();
        assert!(
            matches!(duplicate, BoxliteError::AlreadyExists(_)),
            "{duplicate:?}"
        );
    }

    /// `-v my-data:/data` on a name the store has never seen creates the
    /// volume, as docker does — but only the mount path does; see the next
    /// test.
    #[test]
    fn payload_dir_creates_an_unknown_reference() {
        let tmp = tempfile::tempdir().unwrap();
        let store = LocalNamedVolumeStore::new(tmp.path());

        let dir = store.payload_dir("fresh-vol").unwrap();
        assert!(dir.is_dir());
        let created = store.get("fresh-vol").unwrap();
        assert_eq!(
            tmp.path().join("volumes").join(&created.id).join("_data"),
            dir
        );
        assert_eq!("fresh-vol", created.name);
    }

    /// `docker volume inspect typo` never creates anything; neither may `get`,
    /// or a typo in `boxlite volume get` would leave a stray volume behind.
    #[test]
    fn get_does_not_create_an_unknown_volume() {
        let tmp = tempfile::tempdir().unwrap();
        let store = LocalNamedVolumeStore::new(tmp.path());

        let err = store.get("no-such-volume").unwrap_err();
        assert!(matches!(err, BoxliteError::NotFound(_)), "{err:?}");
        assert!(
            store.list().unwrap().is_empty(),
            "get must not create a volume"
        );
    }

    /// The directory handed to a box must not contain the sidecar: a box could
    /// otherwise rewrite its volume's name and hijack another volume's
    /// `-v name:/path`, or corrupt the file and break every name lookup.
    #[test]
    fn the_payload_directory_never_contains_the_sidecar() {
        let tmp = tempfile::tempdir().unwrap();
        let store = LocalNamedVolumeStore::new(tmp.path());
        store.create(Some("guarded")).unwrap();

        let payload = store.payload_dir("guarded").unwrap();
        assert!(
            !payload.join(".metadata.json").exists(),
            "the sidecar is reachable from the mount: {payload:?}"
        );
        assert!(
            payload.parent().unwrap().join(".metadata.json").is_file(),
            "the sidecar sits beside the payload, in the volume's directory"
        );
    }

    /// A volume removed while a listing is in flight is not an error of the
    /// listing: `volume_info` reports it as gone, `list` moves on, `get` says
    /// not found.
    #[test]
    fn a_directory_that_vanished_mid_scan_is_skipped_not_reported() {
        let tmp = tempfile::tempdir().unwrap();
        let store = LocalNamedVolumeStore::new(tmp.path());
        let created = store.create(Some("vanishing")).unwrap();
        let dir = tmp.path().join("volumes").join(&created.id);
        std::fs::remove_dir_all(&dir).unwrap();

        assert!(
            store
                .volume_info(created.id.clone(), &dir)
                .unwrap()
                .is_none()
        );
        assert!(matches!(
            store.get(&created.id),
            Err(BoxliteError::NotFound(_))
        ));
        assert!(store.list().unwrap().is_empty());
    }

    /// The sidecar is renamed into place, so no reader can see a partial file
    /// and no staging file is left behind.
    #[test]
    fn the_sidecar_is_written_atomically() {
        let tmp = tempfile::tempdir().unwrap();
        let store = LocalNamedVolumeStore::new(tmp.path());
        let created = store.create(Some("atomic")).unwrap();
        let dir = tmp.path().join("volumes").join(&created.id);

        assert!(dir.join(".metadata.json").is_file());
        assert!(!dir.join(".metadata.json.tmp").exists());
        let mut entries: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        entries.sort();
        assert_eq!(vec![".metadata.json", "_data"], entries);
    }

    /// A name that is another volume's id would pass a name-only scan yet
    /// never resolve to the new volume, because lookups try the id first.
    #[test]
    fn a_name_equal_to_another_volumes_id_is_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let store = LocalNamedVolumeStore::new(tmp.path());
        let first = store.create(Some("first")).unwrap();

        let err = store.create(Some(&first.id)).unwrap_err();
        assert!(matches!(err, BoxliteError::AlreadyExists(_)), "{err:?}");
        assert_eq!(1, store.list().unwrap().len());
    }

    /// `remove` mutates the volumes directory like `create`, so it takes the
    /// same lock; a removal racing a first-use mount and a listing of the same
    /// name must never make any of the three fail.
    #[test]
    fn concurrent_remove_mount_and_list_never_break_each_other() {
        use std::sync::{Arc, Barrier};

        type Op = Box<dyn Fn(&LocalNamedVolumeStore) -> BoxliteResult<()> + Send>;
        let tmp = tempfile::tempdir().unwrap();
        let store = Arc::new(LocalNamedVolumeStore::new(tmp.path()));
        for _ in 0..20 {
            store.create(Some("contended")).unwrap();
            let ops: Vec<Op> = vec![
                Box::new(|s| s.remove("contended", true)),
                Box::new(|s| s.payload_dir("contended").map(|_| ())),
                Box::new(|s| s.list().map(|_| ())),
            ];
            let barrier = Arc::new(Barrier::new(ops.len()));
            let handles: Vec<_> = ops
                .into_iter()
                .map(|op| {
                    let store = Arc::clone(&store);
                    let barrier = Arc::clone(&barrier);
                    std::thread::spawn(move || {
                        barrier.wait();
                        op(&store)
                    })
                })
                .collect();
            for handle in handles {
                handle
                    .join()
                    .unwrap()
                    .expect("no operation may fail because another one raced it");
            }
            store.remove("contended", true).unwrap();
        }
    }

    /// Two first-use mounts of the same name at the same time must end up on
    /// one volume: the uniqueness scan and the mkdir behind `create` are two
    /// steps, and only the lock on the volumes directory makes them one.
    #[test]
    fn concurrent_first_use_mounts_share_one_volume() {
        use std::sync::{Arc, Barrier};

        let tmp = tempfile::tempdir().unwrap();
        let store = Arc::new(LocalNamedVolumeStore::new(tmp.path()));
        let threads = 8;
        let barrier = Arc::new(Barrier::new(threads));
        let dirs: Vec<PathBuf> = (0..threads)
            .map(|_| {
                let store = Arc::clone(&store);
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait();
                    store.payload_dir("shared").unwrap()
                })
            })
            .collect::<Vec<_>>()
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect();

        assert!(dirs.iter().all(|dir| dir == &dirs[0]), "{dirs:?}");
        assert_eq!(1, store.list().unwrap().len(), "one volume named shared");
    }

    #[test]
    fn names_outside_the_documented_set_are_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let store = LocalNamedVolumeStore::new(tmp.path());

        for bad in [
            "a",
            "",
            "-leading",
            ".hidden",
            "has space",
            "sl/ash",
            "quo\"te",
        ] {
            assert!(
                matches!(
                    store.create(Some(bad)).unwrap_err(),
                    BoxliteError::InvalidArgument(_)
                ),
                "name {bad:?} must be rejected"
            );
        }
        assert!(store.create(Some("ok-name_1.2")).is_ok());
    }

    #[test]
    fn remove_deletes_payload_and_sidecar_and_force_tolerates_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let store = LocalNamedVolumeStore::new(tmp.path());
        let created = store.create(Some("gone-soon")).unwrap();

        store.remove("gone-soon", false).unwrap();
        assert!(!tmp.path().join("volumes").join(&created.id).exists());
        assert!(
            store.create(Some("gone-soon")).is_ok(),
            "a removed volume must not keep its name reserved"
        );

        let err = store.remove("no-such-volume", false).unwrap_err();
        assert!(matches!(err, BoxliteError::NotFound(_)), "{err:?}");
        store.remove("no-such-volume", true).unwrap();
    }

    #[test]
    fn traversal_ids_are_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let store = LocalNamedVolumeStore::new(tmp.path());
        for bad in ["..", ".", "a/b", "a\\b", "../escape", ""] {
            assert!(
                store.get(bad).is_err(),
                "id {bad:?} must be rejected by get"
            );
            assert!(
                store.remove(bad, true).is_err(),
                "id {bad:?} must be rejected by remove"
            );
        }
    }

    /// The metadata every surface renders from — CLI table, REST body, SDK
    /// object — must not carry the backing directory. See
    /// [`LocalNamedVolumeStore::payload_dir`] for why the runtime asks separately.
    #[test]
    fn volume_info_does_not_carry_the_backing_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let store = LocalNamedVolumeStore::new(tmp.path());
        let created = store.create(None).unwrap();

        let rendered = serde_json::to_value(&created).unwrap();
        assert!(
            !rendered.as_object().unwrap().contains_key("host_path"),
            "volume metadata must not expose the backing directory: {rendered}"
        );

        // Destructuring fails to compile if a field is added, so a path that
        // comes back as a Rust field cannot slip in behind `#[serde(skip)]`.
        let VolumeInfo {
            id: _,
            name: _,
            created_at: _,
            size_bytes: _,
        } = created;
    }
}
