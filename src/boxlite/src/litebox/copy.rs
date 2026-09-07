use std::path::PathBuf;

use boxlite_shared::errors::BoxliteResult;
use boxlite_shared::{BoxByteStream, tar};
use futures::StreamExt;
use tokio::io::AsyncWriteExt;

use crate::BoxliteError;

/// Shape of a streaming copy's source: a directory tree, a single file, or
/// unknown.
///
/// `Unknown` is the honest answer when the producer cannot tell (a peer that
/// predates the hint, or a caller streaming bytes it did not pack). The
/// receiver then peeks the archive to decide the extraction shape, which costs
/// a staged copy — so pass `File`/`Dir` whenever the shape is known.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CopySourceKind {
    Unknown,
    File,
    Dir,
}

impl CopySourceKind {
    /// The wire encoding: proto's `optional bool source_is_dir`.
    pub fn to_wire(self) -> Option<bool> {
        match self {
            Self::Unknown => None,
            Self::File => Some(false),
            Self::Dir => Some(true),
        }
    }

    /// Decode the wire hint; a peer that omits it reports `Unknown`.
    pub fn from_wire(source_is_dir: Option<bool>) -> Self {
        match source_is_dir {
            None => Self::Unknown,
            Some(false) => Self::File,
            Some(true) => Self::Dir,
        }
    }

    pub fn is_dir(self) -> bool {
        matches!(self, Self::Dir)
    }
}

/// Options controlling copy behavior.
#[derive(Debug, Clone)]
pub struct CopyOptions {
    /// Recursively copy directories.
    pub recursive: bool,
    /// Overwrite existing files/directories at destination.
    pub overwrite: bool,
    /// Follow symlinks when archiving (otherwise include symlinks as links).
    pub follow_symlinks: bool,
    /// When copying out, include the parent directory in the archive (docker cp semantics).
    pub include_parent: bool,
}

impl Default for CopyOptions {
    fn default() -> Self {
        Self {
            recursive: true,
            overwrite: true,
            follow_symlinks: false,
            include_parent: true,
        }
    }
}

impl CopyOptions {
    pub fn no_overwrite(mut self) -> Self {
        self.overwrite = false;
        self
    }

    pub fn non_recursive(mut self) -> Self {
        self.recursive = false;
        self
    }

    pub fn follow_symlinks(mut self, follow: bool) -> Self {
        self.follow_symlinks = follow;
        self
    }

    pub fn include_parent(mut self, include: bool) -> Self {
        self.include_parent = include;
        self
    }

    pub fn validate_for_dir(&self) -> Result<(), BoxliteError> {
        if !self.recursive {
            return Err(BoxliteError::Config(
                "recursive=false not supported for directory copies".into(),
            ));
        }
        Ok(())
    }
}

/// Where a shapeless copy-out may stage its archive, and how much of it.
///
/// Staging is bounded rather than unbounded: past `cap_bytes` the copy is
/// refused with the advice to upgrade the peer, which is the only way out of
/// the fallback.
pub(crate) struct SpoolPolicy {
    /// Directory the temp archive is created in. Explicit rather than
    /// `env::temp_dir()` so a caller with a sized scratch volume can name it.
    pub dir: PathBuf,
    pub cap_bytes: u64,
}

/// Unpack a tar stream whose source shape the peer did not report.
///
/// [`tar::unpack_stream`] takes the extraction shape authoritatively from
/// `force_directory`, so a [`CopySourceKind::Unknown`] transfer has no honest
/// bool to hand it — and guessing `false` extracts a directory archive as a
/// single file. Stage the archive instead and let [`tar::unpack`] peek it, at
/// the cost of a temp file. `TempPath` owns the deletion because every step
/// from here on can leave through a `?`.
pub(crate) async fn unpack_stream_spooled(
    mut stream: BoxByteStream,
    dest: PathBuf,
    opts: tar::UnpackContext,
    policy: SpoolPolicy,
) -> BoxliteResult<()> {
    let staged = tempfile::Builder::new()
        .prefix("cp-out-spool-")
        .suffix(".tar")
        .tempfile_in(&policy.dir)
        .map_err(|e| BoxliteError::Storage(format!("failed to stage copy archive: {e}")))?
        .into_temp_path();

    let mut file = tokio::fs::File::create(&staged)
        .await
        .map_err(|e| BoxliteError::Storage(format!("failed to open staged archive: {e}")))?;
    let mut staged_bytes: u64 = 0;
    while let Some(item) = stream.next().await {
        let chunk =
            item.map_err(|e| BoxliteError::Storage(format!("failed to read copy stream: {e}")))?;
        staged_bytes += chunk.len() as u64;
        if staged_bytes > policy.cap_bytes {
            return Err(BoxliteError::Unsupported(format!(
                "copy_out exceeds the {} MiB fallback cap; the peer omits the archive-shape \
                 hint, so the archive must be staged — upgrade the peer to stream end-to-end",
                policy.cap_bytes / (1024 * 1024)
            )));
        }
        file.write_all(&chunk)
            .await
            .map_err(|e| BoxliteError::Storage(format!("failed to write staged archive: {e}")))?;
    }
    file.flush()
        .await
        .map_err(|e| BoxliteError::Storage(format!("failed to flush staged archive: {e}")))?;
    drop(file);

    tar::unpack(staged.to_path_buf(), dest, opts).await
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The guest reads `source_is_dir` to pick the extraction shape, so an
    /// inverted arm here silently unpacks a directory as a file (or the
    /// reverse) on every peer — Rust, Go and C alike, since all three encode
    /// through this one pair. Pin the encoding itself, not just the round trip:
    /// a round trip survives a consistently inverted mapping.
    #[test]
    fn source_kind_encodes_the_guest_protocol_hint() {
        assert_eq!(CopySourceKind::Unknown.to_wire(), None);
        assert_eq!(CopySourceKind::File.to_wire(), Some(false));
        assert_eq!(CopySourceKind::Dir.to_wire(), Some(true));

        assert_eq!(CopySourceKind::from_wire(None), CopySourceKind::Unknown);
        assert_eq!(CopySourceKind::from_wire(Some(false)), CopySourceKind::File);
        assert_eq!(CopySourceKind::from_wire(Some(true)), CopySourceKind::Dir);
    }

    /// Only `Dir` makes the caller validate recursion — an `Unknown` source
    /// must not be treated as a directory before the guest has peeked.
    #[test]
    fn only_dir_reports_a_directory_source() {
        assert!(CopySourceKind::Dir.is_dir());
        assert!(!CopySourceKind::File.is_dir());
        assert!(!CopySourceKind::Unknown.is_dir());
    }

    /// Collect a real archive from the packer production uses, so the bytes
    /// under test are the ones a copy actually moves.
    async fn packed(src: std::path::PathBuf) -> (bool, Vec<u8>) {
        let (is_dir, stream) = tar::pack_stream(
            src,
            tar::PackContext {
                follow_symlinks: false,
                include_parent: false,
            },
        )
        .await
        .expect("pack");
        let mut bytes = Vec::new();
        let mut stream = std::pin::pin!(stream);
        while let Some(chunk) = stream.next().await {
            bytes.extend_from_slice(&chunk.expect("chunk"));
        }
        (is_dir, bytes)
    }

    fn stream_of(bytes: Vec<u8>) -> BoxByteStream {
        Box::pin(tokio_stream::iter(vec![Ok(bytes)]))
    }

    fn unpack_opts() -> tar::UnpackContext {
        tar::UnpackContext {
            overwrite: true,
            mkdir_parents: true,
            // What a peer that omits the shape hint leaves us with: no honest
            // value, so the spooled arm has to peek instead of trusting this.
            force_directory: false,
        }
    }

    /// A directory archive whose destination does not exist yet has no
    /// destination-side signal to read, so the shape can only come from the
    /// archive. The spooled arm peeks and lands a tree.
    #[tokio::test]
    async fn spooled_unpack_peeks_the_shape_a_hintless_peer_did_not_send() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("tree");
        std::fs::create_dir(&src).unwrap();
        std::fs::write(src.join("a.txt"), b"a").unwrap();
        std::fs::write(src.join("b.txt"), b"b").unwrap();
        let (is_dir, bytes) = packed(src).await;
        assert!(is_dir, "the fixture is a directory");

        let dest = tmp.path().join("landed");
        unpack_stream_spooled(
            stream_of(bytes),
            dest.clone(),
            unpack_opts(),
            SpoolPolicy {
                dir: tmp.path().to_path_buf(),
                cap_bytes: 1 << 20,
            },
        )
        .await
        .expect("spooled unpack");

        assert!(
            dest.is_dir(),
            "a directory archive must land as a directory"
        );
        assert_eq!(std::fs::read(dest.join("a.txt")).unwrap(), b"a");
        assert_eq!(std::fs::read(dest.join("b.txt")).unwrap(), b"b");
    }

    /// Why the spool exists at all: the same archive through the streaming
    /// unpack, which cannot peek, extracts the first entry *as* the
    /// destination file. Pinning it keeps someone from "simplifying" the
    /// `Unknown` arm into a `force_directory: false` stream.
    #[tokio::test]
    async fn streaming_unpack_cannot_recover_the_shape_and_lands_a_file() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("tree");
        std::fs::create_dir(&src).unwrap();
        std::fs::write(src.join("a.txt"), b"a").unwrap();
        let (_, bytes) = packed(src).await;

        let dest = tmp.path().join("landed");
        tar::unpack_stream(stream_of(bytes), dest.clone(), unpack_opts())
            .await
            .expect("streaming unpack");

        assert!(
            dest.is_file(),
            "without the hint the streaming unpack has to guess, and guesses file"
        );
    }

    /// The spool is bounded: a hintless peer cannot make the host stage an
    /// archive of any size. Past the cap the copy is refused, and the temp
    /// archive goes with it.
    #[tokio::test]
    async fn spooled_unpack_refuses_past_its_cap_and_stages_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("big.txt");
        std::fs::write(&src, vec![b'x'; 64 * 1024]).unwrap();
        let (_, bytes) = packed(src).await;
        let spool = tmp.path().join("spool");
        std::fs::create_dir(&spool).unwrap();

        let error = unpack_stream_spooled(
            stream_of(bytes),
            tmp.path().join("landed"),
            unpack_opts(),
            SpoolPolicy {
                dir: spool.clone(),
                cap_bytes: 4 * 1024,
            },
        )
        .await
        .expect_err("an archive past the cap must be refused");

        assert!(
            matches!(error, BoxliteError::Unsupported(_)),
            "the way out is a peer that streams, not a bigger buffer: {error:?}"
        );
        assert!(
            error.to_string().contains("fallback cap"),
            "the refusal must say which limit it hit: {error}"
        );
        assert_eq!(
            std::fs::read_dir(&spool).unwrap().count(),
            0,
            "the staged archive must not outlive the refusal"
        );
    }
}
