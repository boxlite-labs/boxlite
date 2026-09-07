//! File upload/download handlers.
//!
//! Both directions relay the tar stream between the HTTP body and the box
//! without staging it: the archive never lands on the server's disk and never
//! sits in its memory whole. The runner's Go handlers
//! (apps/runner/pkg/api/controllers/boxlite_files.go) are the behavioural spec
//! — these answer the same clients on the same wire contract.

use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use futures::StreamExt;

use boxlite::{CopyOptions, CopySourceKind};

use super::super::types::FileQuery;
use super::super::{AppState, error_from_boxlite, error_response, get_or_resume_box};

/// The response header carrying the archive shape to the client.
const SOURCE_IS_DIR_HEADER: &str = "X-Boxlite-Source-Is-Dir";

/// Read the client's archive-shape hint.
///
/// Absent or unparseable means [`CopySourceKind::Unknown`] — the guest then
/// peeks the archive itself, which is exactly how it behaved before the hint
/// existed. Accepts the set Go's `strconv.ParseBool` accepts, because the
/// runner parses the same query with it and the two servers must not disagree
/// about what a client said.
fn parse_source_is_dir(raw: Option<&str>) -> CopySourceKind {
    match raw {
        Some("1" | "t" | "T" | "true" | "TRUE" | "True") => CopySourceKind::Dir,
        Some("0" | "f" | "F" | "false" | "FALSE" | "False") => CopySourceKind::File,
        _ => CopySourceKind::Unknown,
    }
}

pub(in crate::commands::serve) async fn upload_files(
    State(state): State<Arc<AppState>>,
    Path(box_id): Path<String>,
    Query(query): Query<FileQuery>,
    body: Body,
) -> Response {
    let litebox = match get_or_resume_box(&state, &box_id).await {
        Ok(b) => b,
        Err(resp) => return resp,
    };

    // Straight from the request body into the box. `Body` (rather than
    // `Bytes`) is also what lifts axum's 2 MiB extractor limit, which used to
    // reject any archive bigger than that with a 413.
    let source = parse_source_is_dir(query.source_is_dir.as_deref());
    let archive = body
        .into_data_stream()
        .map(|chunk| chunk.map(|b| b.to_vec()).map_err(std::io::Error::other));

    if let Err(e) = litebox
        .copy_in_stream(archive, &query.path, source, CopyOptions::default())
        .await
    {
        return error_from_boxlite(&e);
    }

    StatusCode::NO_CONTENT.into_response()
}

pub(in crate::commands::serve) async fn download_files(
    State(state): State<Arc<AppState>>,
    Path(box_id): Path<String>,
    Query(query): Query<FileQuery>,
) -> Response {
    let litebox = match get_or_resume_box(&state, &box_id).await {
        Ok(b) => b,
        Err(resp) => return resp,
    };

    // The guest's first chunk is read before this returns, so the shape is in
    // hand before a single response byte is committed — no callback needed to
    // set the header in time, and a refusal (missing source, a shadowing
    // mount) still answers with a status instead of a truncated 200.
    let (archive, source) = match litebox
        .copy_out_stream(&query.path, CopyOptions::default())
        .await
    {
        Ok(pair) => pair,
        Err(e) => return error_from_boxlite(&e),
    };

    let mut response = Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "application/x-tar");
    // Omitted, never guessed, when the guest itself could not tell: a client
    // that sees no header runs its own shape detection, while a fabricated
    // `false` would make it extract a directory archive as a single file.
    if let Some(is_dir) = source.to_wire() {
        response = response.header(SOURCE_IS_DIR_HEADER, is_dir.to_string());
    }

    // A mid-stream failure must *sever* the response, not end the body
    // cleanly: a tar cut on a 512-byte block boundary extracts without error,
    // just missing entries, so a clean end would look like a whole archive. A
    // terminal `Err` frame makes hyper drop the connection without writing the
    // chunked terminator, which is what the runner achieves with
    // `panic(http.ErrAbortHandler)`. Panicking here would not: `CatchPanicLayer`
    // wraps body polling and would turn it into a 500 after a 200 is committed.
    let path = query.path.clone();
    let body = Body::from_stream(archive.map(move |chunk| {
        chunk.inspect_err(|e| {
            tracing::error!(
                box_id = %box_id,
                path = %path,
                error = %e,
                "copy_out failed mid-stream, severing the response"
            );
        })
    }));

    match response.body(body) {
        Ok(response) => response,
        Err(e) => error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("failed to build the download response: {e}"),
            "InternalError",
            "internal",
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The runner parses this query with Go's `strconv.ParseBool`, and
    /// `TestParseSourceIsDir` (apps/runner/pkg/api/controllers/
    /// boxlite_files_test.go) pins its answers for the values a client is
    /// likely to send. A client that reaches one server must not get a
    /// different extraction shape from the other, so this covers the same
    /// values and then the rest of what `ParseBool` accepts.
    #[test]
    fn source_is_dir_accepts_what_the_runner_accepts() {
        for raw in ["1", "t", "T", "true", "TRUE", "True"] {
            assert_eq!(parse_source_is_dir(Some(raw)), CopySourceKind::Dir, "{raw}");
        }
        for raw in ["0", "f", "F", "false", "FALSE", "False"] {
            assert_eq!(
                parse_source_is_dir(Some(raw)),
                CopySourceKind::File,
                "{raw}"
            );
        }
    }

    /// An old client sends no hint at all, and a malformed one is not a reason
    /// to fail the copy — both mean "the guest decides", its pre-hint
    /// behaviour. Answering `File` instead would extract a directory archive
    /// as a single file.
    #[test]
    fn a_missing_or_malformed_hint_is_unknown() {
        assert_eq!(parse_source_is_dir(None), CopySourceKind::Unknown);
        for raw in ["", "bogus", "2", "yes", "dir"] {
            assert_eq!(
                parse_source_is_dir(Some(raw)),
                CopySourceKind::Unknown,
                "{raw}"
            );
        }
    }
}
