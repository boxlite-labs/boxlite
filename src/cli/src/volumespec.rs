//! Parsing for `-v`/`--volume` mount specs.
//!
//! One token has to say which of three things the caller meant, so the rule is
//! Docker's, character for character
//! (`docker/cli/internal/volumespec/volumespec.go:33-49` for the scanner,
//! `:96-124` for the classification):
//!
//! - The spec is split by scanning, not by `split(':')`. A colon preceded by
//!   exactly one letter is a Windows drive letter and is absorbed into the
//!   field rather than ending it, so `C:\data:/app` splits into two fields and
//!   not three.
//! - The source field is then classified by its **first character**: `.`, `/`,
//!   `~`, a `\\` prefix, or `X:` mean a host path. Anything else is a managed
//!   volume, addressed by id or by name.
//!
//! What an unknown reference does depends on the runtime. The local runtime
//! creates the volume on first use, as docker does, so `-v data:/app` works
//! without a prior `volume create`; a REST server answers "not found"
//! (`VolumeService.validateVolumes`), so a mistyped name cannot silently
//! create an empty volume there.
//!
//! Unlike Docker this classification happens exactly once. Docker re-derives it
//! daemon-side from an untyped string (`moby/daemon/volume/mounts/linux_parser.go`,
//! `ParseMountRaw` re-testing `path.IsAbs`); `VolumeSpec` carries
//! `managed_volume` and `host_path` as separate fields, so the decision made
//! here survives all the way to the wire.

/// Where a parsed mount's contents come from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MountOrigin {
    /// A managed volume, by server-assigned id or by name.
    ManagedVolume(String),
    /// A host directory or file. Relative paths are resolved by the caller.
    BindMount(String),
    /// No source given — the caller wants scratch space at `guest_path`.
    Anonymous,
}

/// One `-v` spec, resolved into an origin plus its mount point.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedMount {
    pub origin: MountOrigin,
    pub guest_path: String,
    pub read_only: bool,
}

/// Parse one `-v` value.
///
/// Grammar, after the drive-aware split:
/// - `BOX_PATH` / `BOX_PATH:ro` — anonymous volume
/// - `SOURCE:BOX_PATH[:OPTIONS]` — managed volume or host bind, per `classify`
pub fn parse(spec: &str) -> anyhow::Result<ParsedMount> {
    let spec = spec.trim();
    if spec.is_empty() {
        anyhow::bail!("empty volume spec");
    }

    let fields = split_fields(spec);
    let fields: Vec<&str> = fields.iter().map(|f| f.trim()).collect();

    match fields.as_slice() {
        [guest] => Ok(ParsedMount {
            origin: MountOrigin::Anonymous,
            guest_path: absolute_box_path(guest)?,
            read_only: false,
        }),

        // `BOX_PATH:ro` is an anonymous volume with a mode, not a source named
        // "ro". Checked before the source form so the shorthand keeps working.
        [guest, mode] if is_mode(mode) => Ok(ParsedMount {
            origin: MountOrigin::Anonymous,
            guest_path: absolute_box_path(guest)?,
            read_only: mode.eq_ignore_ascii_case("ro"),
        }),

        [source, guest] => Ok(ParsedMount {
            origin: classify(source)?,
            guest_path: absolute_box_path(guest)?,
            read_only: false,
        }),

        [source, guest, options] => Ok(ParsedMount {
            origin: classify(source)?,
            guest_path: absolute_box_path(guest)?,
            read_only: parse_read_only(options),
        }),

        _ => anyhow::bail!(
            "invalid volume spec {spec:?}; use VOLUME:BOX_PATH, HOST_PATH:BOX_PATH[:OPTIONS], \
             or BOX_PATH[:OPTIONS] for an anonymous volume"
        ),
    }
}

/// Split on `:`, except where the colon is a Windows drive separator.
///
/// Mirrors the scanner in `volumespec.go`: a colon terminates a field unless
/// the field so far is exactly one letter, in which case it belongs to the
/// field. That is what keeps `C:\data` whole without counting colons.
///
/// Upstream: `docker/cli/internal/volumespec/volumespec.go:33-49`.
fn split_fields(spec: &str) -> Vec<String> {
    let mut fields = Vec::new();
    let mut buffer = String::new();

    for ch in spec.chars() {
        if ch == ':' && !is_drive_letter(&buffer) {
            fields.push(std::mem::take(&mut buffer));
        } else {
            buffer.push(ch);
        }
    }
    fields.push(buffer);

    fields
}

/// A field holding exactly one ASCII letter — the left half of `C:`.
fn is_drive_letter(buffer: &str) -> bool {
    let mut chars = buffer.chars();
    matches!((chars.next(), chars.next()), (Some(c), None) if c.is_ascii_alphabetic())
}

/// Decide whether a source names a host path or a managed volume.
///
/// First character only, as `isFilePath` does
/// (`docker/cli/internal/volumespec/volumespec.go:108-124`). Nothing here
/// inspects the filesystem:
/// a spec must mean the same thing on every machine, whether or not the path
/// happens to exist.
fn classify(source: &str) -> anyhow::Result<MountOrigin> {
    if source.is_empty() {
        anyhow::bail!("volume source must be non-empty");
    }

    let host_path = match source.chars().next() {
        Some('.') | Some('/') | Some('~') => true,
        // UNC path or Windows named pipe.
        _ if source.starts_with(r"\\") => true,
        _ => is_windows_drive_prefix(source),
    };

    Ok(if host_path {
        MountOrigin::BindMount(source.to_string())
    } else {
        MountOrigin::ManagedVolume(source.to_string())
    })
}

/// `C:\data` or `C:/data` — a drive letter, a colon, then a separator.
///
/// Public to the crate so the host-path resolver shares this one definition:
/// on Unix `Path::is_relative` calls `C:\data` relative and would canonicalize
/// it against the working directory.
pub fn is_windows_drive_prefix(source: &str) -> bool {
    let bytes = source.as_bytes();
    bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && (bytes[2] == b'\\' || bytes[2] == b'/')
}

fn is_mode(field: &str) -> bool {
    field.eq_ignore_ascii_case("ro") || field.eq_ignore_ascii_case("rw")
}

/// Read `ro` out of an options list (`ro`, `rw,nocopy`). Other options are
/// ignored, matching the behaviour this replaces.
fn parse_read_only(options: &str) -> bool {
    options
        .split(',')
        .any(|option| option.trim().eq_ignore_ascii_case("ro"))
}

/// The mount point inside the box. Always POSIX-absolute: guests are Linux, so
/// unlike Docker there is no Windows-destination case to allow for.
fn absolute_box_path(guest: &str) -> anyhow::Result<String> {
    if guest.is_empty() {
        anyhow::bail!("volume box path must be non-empty");
    }
    if !guest.starts_with('/') {
        anyhow::bail!("volume box path must be absolute (e.g. /data), got {guest:?}");
    }
    Ok(guest.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn managed(spec: &str) -> String {
        match parse(spec).unwrap().origin {
            MountOrigin::ManagedVolume(volume) => volume,
            other => panic!("{spec:?} should be a managed volume, got {other:?}"),
        }
    }

    fn host(spec: &str) -> String {
        match parse(spec).unwrap().origin {
            MountOrigin::BindMount(path) => path,
            other => panic!("{spec:?} should be a host path, got {other:?}"),
        }
    }

    #[test]
    fn bare_source_is_a_managed_volume() {
        assert_eq!(managed("my-data:/data"), "my-data");
        assert_eq!(managed("vol_01K2EXAMPLE:/data"), "vol_01K2EXAMPLE");
        assert_eq!(managed("data:/data"), "data");
    }

    /// A one-letter source is unreachable, and deliberately so: the scanner
    /// cannot tell `a:` from a drive letter, so it absorbs the colon and the
    /// whole spec becomes a single field — which then fails as a non-absolute
    /// box path. Docker's scanner behaves identically (`isWindowsDrive` tests
    /// only that the buffer is one letter). Single-character volume names are
    /// therefore not addressable via `-v`; Docker rejects them outright with
    /// "volume name is too short".
    #[test]
    fn single_letter_source_is_swallowed_as_a_drive_letter() {
        let error = parse("a:/data").unwrap_err().to_string();
        assert!(error.contains("must be absolute"), "{error}");
    }

    #[test]
    fn leading_dot_slash_or_tilde_is_a_host_path() {
        assert_eq!(host("/host/data:/data"), "/host/data");
        assert_eq!(host("./data:/data"), "./data");
        assert_eq!(host("../data:/data"), "../data");
        assert_eq!(host("~/data:/data"), "~/data");
    }

    /// The case that broke an earlier attempt at this: the drive colon must not
    /// end the field, or `C` is read as a volume named "C".
    #[test]
    fn windows_drive_paths_survive_the_split() {
        assert_eq!(host(r"C:\data:/data"), r"C:\data");
        assert_eq!(host(r"D:/host/path:/data"), r"D:/host/path");
        assert_eq!(host(r"\\server\share:/data"), r"\\server\share");
    }

    #[test]
    fn windows_drive_path_still_takes_options() {
        let mount = parse(r"C:\data:/data:ro").unwrap();
        assert_eq!(mount.origin, MountOrigin::BindMount(r"C:\data".to_string()));
        assert_eq!(mount.guest_path, "/data");
        assert!(mount.read_only);
    }

    #[test]
    fn anonymous_forms() {
        assert_eq!(parse("/data").unwrap().origin, MountOrigin::Anonymous);
        let read_only = parse("/data:ro").unwrap();
        assert_eq!(read_only.origin, MountOrigin::Anonymous);
        assert!(read_only.read_only);
        assert!(!parse("/data:rw").unwrap().read_only);
    }

    #[test]
    fn read_only_option_is_parsed_for_both_origins() {
        assert!(parse("my-data:/data:ro").unwrap().read_only);
        assert!(parse("/host:/data:ro").unwrap().read_only);
        assert!(!parse("/host:/data:rw,nocopy").unwrap().read_only);
        assert!(!parse("/host:/data:rw").unwrap().read_only);
    }

    #[test]
    fn box_path_must_be_absolute() {
        for spec in ["my-data:data", "/host:data", "data"] {
            let error = parse(spec).unwrap_err().to_string();
            assert!(error.contains("must be absolute"), "{spec}: {error}");
        }
    }

    #[test]
    fn rejects_empty_and_overlong_specs() {
        assert!(parse("").unwrap_err().to_string().contains("empty"));
        assert!(parse("   ").unwrap_err().to_string().contains("empty"));
        let error = parse("a:/b:ro:extra:more").unwrap_err().to_string();
        assert!(error.contains("invalid volume spec"), "{error}");
    }
}
