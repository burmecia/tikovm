//! Crate-wide error type for `vmtop`.

use thiserror::Error;

/// Errors returned by the tool: terminal/IO problems and API requests.
#[derive(Debug, Error)]
pub(crate) enum Error {
    /// A request to hostd failed before producing a response (DNS, connect,
    /// timeout) or its body could not be parsed.
    #[error("api request failed: {0}")]
    Api(String),
    /// hostd answered with a non-2xx status. Errors use the uniform body
    /// `{"error": {"code": <status>, "message": ...}}`; prefer `message`
    /// when it is present.
    #[error("hostd returned {status}: {body}")]
    Http { status: u16, body: String },
    /// A generic I/O failure while operating the terminal or a crossterm
    /// read/write (crossterm exposes `io::Error`).
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    /// The API bearer token was not supplied (missing or empty).
    #[error("missing API token: set TIKOVM_HOSTD_API_TOKEN or pass --token")]
    MissingToken,
}

pub(crate) type Result<T> = std::result::Result<T, Error>;
