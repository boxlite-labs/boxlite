use boxlite_shared::errors::BoxliteError;
use napi::Error as NapiError;

/// Map BoxliteError to napi Error
/// This is the single source of truth for error conversion (DRY principle)
pub(crate) fn map_err(err: BoxliteError) -> NapiError {
    NapiError::from_reason(format!("{}", err))
}
