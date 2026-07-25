use crate::common::vm::VmState;

#[derive(Debug, thiserror::Error)]
pub(crate) enum Error {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("JSON parse error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("Lock error: {0}")]
    Lock(String),

    #[error("tracing_subscriber filter parse: {0}")]
    TracingSubscriberFilterParse(#[from] tracing_subscriber::filter::ParseError),

    #[error("vmm error: {0}")]
    Vmm(String),

    #[error("vm {0} not found")]
    VmNotFound(String),

    #[error("invalid VM state transition: {from:?} -> {to:?}")]
    InvalidStateTransition { from: VmState, to: VmState },

    #[error("HOSTD_API_TOKEN environment variable must be set to a non-empty value")]
    MissingApiToken,
    //#[error("{0}")]
    //Other(String),
}

impl Error {
    pub(crate) fn io_other(msg: impl Into<String>) -> Self {
        Error::Io(std::io::Error::new(std::io::ErrorKind::Other, msg.into()))
    }

    pub(crate) fn vmm(msg: impl Into<String>) -> Self {
        Error::Vmm(msg.into())
    }
}

impl<T> From<std::sync::PoisonError<T>> for Error {
    fn from(err: std::sync::PoisonError<T>) -> Self {
        Error::Lock(err.to_string())
    }
}

pub(crate) type Result<T> = std::result::Result<T, Error>;
