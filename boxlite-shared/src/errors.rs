//! Error types used across the Boxlite runtime.

use thiserror::Error;

/// Result type for Boxlite operations.
pub type BoxliteResult<T> = Result<T, BoxliteError>;

#[derive(Debug, Error)]
pub enum BoxliteError {
    #[error("unsupported engine kind")]
    UnsupportedEngine,

    #[error("engine reported an error: {0}")]
    Engine(String),

    #[error("configuration error: {0}")]
    Config(String),

    #[error("storage error: {0}")]
    Storage(String),

    #[error("images error: {0}")]
    Image(String),

    #[error("portal error: {0}")]
    Portal(String),

    #[error("network error: {0}")]
    Network(String),

    #[error("gRPC/tonic error: {0}")]
    Rpc(String),

    #[error("gRPC transport error: {0}")]
    RpcTransport(String),

    #[error("internal error: {0}")]
    Internal(String),

    #[error("Execution error: {0}")]
    Execution(String),
}

// Implement From for common error types to enable `?` operator
impl From<std::io::Error> for BoxliteError {
    fn from(err: std::io::Error) -> Self {
        BoxliteError::Internal(format!("I/O error: {}", err))
    }
}

impl From<serde_json::Error> for BoxliteError {
    fn from(err: serde_json::Error) -> Self {
        BoxliteError::Internal(format!("JSON error: {}", err))
    }
}

impl From<String> for BoxliteError {
    fn from(err: String) -> Self {
        BoxliteError::Internal(err)
    }
}

impl From<&str> for BoxliteError {
    fn from(err: &str) -> Self {
        BoxliteError::Internal(err.to_string())
    }
}

impl From<tonic::Status> for BoxliteError {
    fn from(err: tonic::Status) -> Self {
        BoxliteError::Rpc(err.to_string())
    }
}

impl From<tonic::transport::Error> for BoxliteError {
    fn from(err: tonic::transport::Error) -> Self {
        BoxliteError::RpcTransport(err.to_string())
    }
}
