//! `boxlite serve`'s file routes, driven end to end: `boxlite --url … cp`
//! (the REST client) → the axum handlers → the local backend → the guest.
//!
//! One box for every case — a VM boot per assertion is the expensive part.
//!
//! ```sh
//! cargo test -p boxlite-cli --test serve_files_stream -- --nocapture
//! ```

mod common;

use common::serve::ServeChild;
use std::io::{Read, Write};
use std::path::Path;

/// Bigger than axum's 2 MiB extractor limit, which the upload handler used to
/// sit behind: every archive past it came back as a 413 before the body was
/// streamed.
const PAYLOAD_BYTES: usize = 4 * 1024 * 1024;

fn create_box(serve: &ServeChild) -> String {
    let out = serve.client(&["create", "alpine:latest", "sh", "-c", "sleep 300"]);
    assert!(
        out.status.success(),
        "create failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let id = String::from_utf8_lossy(&out.stdout).trim().to_string();

    let started = serve.client(&["start", &id]);
    assert!(
        started.status.success(),
        "start failed: {}",
        String::from_utf8_lossy(&started.stderr)
    );
    id
}

fn remove_box(serve: &ServeChild, id: &str) {
    let _ = serve.client(&["rm", "-f", id]);
    serve.wait_until_no_boxes();
}

/// A payload that cannot be mistaken for a run of zeroes a bug happened to
/// produce — every 251-byte window is distinct, so a truncated or reordered
/// transfer shows up as a byte mismatch.
fn payload() -> Vec<u8> {
    (0..PAYLOAD_BYTES).map(|i| (i % 251) as u8).collect()
}

/// Issue a raw HTTP/1.1 request and return `(status_line, headers)`.
///
/// Raw TCP rather than an HTTP client: the assertion is about the wire — a
/// header the Rust REST client reads to decide whether it can stream — and no
/// HTTP client is a dependency of this crate.
fn raw_get_headers(port: u16, path: &str) -> (String, Vec<String>) {
    let mut socket = std::net::TcpStream::connect(("127.0.0.1", port)).expect("connect to serve");
    socket
        .write_all(
            format!(
                "GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nAccept: application/x-tar\r\nConnection: close\r\n\r\n"
            )
            .as_bytes(),
        )
        .expect("write request");

    let mut raw = Vec::new();
    socket.read_to_end(&mut raw).expect("read response");
    let text = String::from_utf8_lossy(&raw);
    let head = text.split("\r\n\r\n").next().unwrap_or_default();
    let mut lines = head.lines();
    let status = lines.next().unwrap_or_default().to_string();
    (status, lines.map(str::to_string).collect())
}

/// PUT a body and return the status line, mirroring `raw_get_headers`.
fn raw_put(port: u16, path: &str, body: &[u8]) -> String {
    let mut socket = std::net::TcpStream::connect(("127.0.0.1", port)).expect("connect to serve");
    socket
        .write_all(
            format!(
                "PUT {path} HTTP/1.1\r\nHost: 127.0.0.1\r\n\
                 Content-Type: application/x-tar\r\nContent-Length: {}\r\n\
                 Connection: close\r\n\r\n",
                body.len()
            )
            .as_bytes(),
        )
        .expect("write request head");
    socket.write_all(body).expect("write body");

    let mut raw = Vec::new();
    socket.read_to_end(&mut raw).expect("read response");
    String::from_utf8_lossy(&raw)
        .lines()
        .next()
        .unwrap_or_default()
        .to_string()
}

fn header<'a>(headers: &'a [String], name: &str) -> Option<&'a str> {
    let prefix = format!("{}:", name.to_ascii_lowercase());
    headers
        .iter()
        .find(|line| line.to_ascii_lowercase().starts_with(&prefix))
        .map(|line| line.split_once(':').unwrap().1.trim())
}

/// A multi-megabyte file must survive `cp` in and back out through the server.
///
/// The reproducer for the staged handler: it read the whole body with the
/// `Bytes` extractor, so anything past axum's 2 MiB default came back 413,
/// and what did fit was unpacked to a temp dir and re-packed on the way in.
fn multi_megabyte_roundtrip(serve: &ServeChild, box_id: &str, tmp: &Path) {
    let source = tmp.join("payload.bin");
    let bytes = payload();
    std::fs::write(&source, &bytes).unwrap();

    let up = serve.client(&[
        "cp",
        source.to_str().unwrap(),
        &format!("{box_id}:/payload.bin"),
    ]);
    assert!(
        up.status.success(),
        "copy in failed: {}",
        String::from_utf8_lossy(&up.stderr)
    );

    let landed = tmp.join("returned.bin");
    let down = serve.client(&[
        "cp",
        &format!("{box_id}:/payload.bin"),
        landed.to_str().unwrap(),
    ]);
    assert!(
        down.status.success(),
        "copy out failed: {}",
        String::from_utf8_lossy(&down.stderr)
    );

    let returned = std::fs::read(&landed).expect("the returned payload must exist");
    assert_eq!(
        returned.len(),
        bytes.len(),
        "the returned payload changed size"
    );
    assert_eq!(returned, bytes, "the returned payload's bytes differ");
}

/// The server must tell the client the archive's shape.
///
/// Without the header the client cannot stream at all: it falls back to
/// buffering the whole archive under a 512 MiB cap to peek at it, and refuses
/// outright past that. The value has to be the `true`/`false` the client
/// parses with `str::parse::<bool>()`, so `1`/`0` would not do.
fn download_reports_the_archive_shape(serve: &ServeChild, box_id: &str) {
    let (status, headers) = raw_get_headers(
        serve.port(),
        &format!("/v1/boxes/{box_id}/files?path=/payload.bin"),
    );
    assert!(status.contains("200"), "unexpected status: {status}");
    assert_eq!(
        header(&headers, "X-Boxlite-Source-Is-Dir"),
        Some("false"),
        "a single file must be reported as such, headers: {headers:?}"
    );

    let (status, headers) = raw_get_headers(
        serve.port(),
        &format!("/v1/boxes/{box_id}/files?path=/etc/network"),
    );
    assert!(status.contains("200"), "unexpected status: {status}");
    assert_eq!(
        header(&headers, "X-Boxlite-Source-Is-Dir"),
        Some("true"),
        "a directory must be reported as such, headers: {headers:?}"
    );
}

/// A client that predates the shape hint sends no `source_is_dir` at all.
///
/// The server must not reject it, and must not guess `false` either: a
/// directory archive extracted in single-file mode lands one entry and drops
/// the rest. The box peeks the archive instead — its pre-hint behaviour.
fn a_client_without_the_shape_hint_still_lands_a_tree(serve: &ServeChild, box_id: &str) {
    // Two entries under a directory, so extracting in file mode would be
    // visibly wrong rather than coincidentally right.
    let mut archive = Vec::new();
    {
        let mut builder = tar::Builder::new(&mut archive);
        for (name, content) in [
            ("hinted/a.txt", &b"aaa\n"[..]),
            ("hinted/b.txt", &b"bbb\n"[..]),
        ] {
            let mut header = tar::Header::new_gnu();
            header.set_size(content.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            builder.append_data(&mut header, name, content).unwrap();
        }
        builder.finish().unwrap();
    }

    let status = raw_put(
        serve.port(),
        &format!("/v1/boxes/{box_id}/files?path=/oldclient"),
        &archive,
    );
    assert!(
        status.contains("204"),
        "an upload without the hint must be accepted: {status}"
    );

    let listed = serve.client(&["exec", box_id, "--", "ls", "/oldclient/hinted"]);
    let listed = String::from_utf8_lossy(&listed.stdout);
    assert!(
        listed.contains("a.txt") && listed.contains("b.txt"),
        "both entries must land, got: {listed:?}"
    );
}

#[test]
fn serve_streams_file_copies() {
    let serve = ServeChild::start();
    let tmp = tempfile::tempdir().expect("tempdir");
    let box_id = create_box(&serve);

    multi_megabyte_roundtrip(&serve, &box_id, tmp.path());
    download_reports_the_archive_shape(&serve, &box_id);
    a_client_without_the_shape_hint_still_lands_a_tree(&serve, &box_id);

    remove_box(&serve, &box_id);
}
