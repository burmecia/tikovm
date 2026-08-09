use crate::vmm::vm::VmState;
use std::sync::PoisonError;

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

    #[error("net error: {0}")]
    Net(String),

    #[error("storage error: {0}")]
    Storage(String),

    #[error("vm {0} not found")]
    VmNotFound(String),

    #[error("workload {0} not found")]
    WorkloadNotFound(String),

    #[error("invalid VM image: {0}")]
    InvalidImage(String),

    #[error("invalid VM state transition: {from:?} -> {to:?}")]
    InvalidStateTransition { from: VmState, to: VmState },

    #[error("invalid port {0}: must be 1-65535")]
    InvalidPort(u16),

    #[error("port {port} is already exposed on vm {vm_id}")]
    PortAlreadyExposed { vm_id: String, port: u16 },

    #[error("port {port} is not exposed on vm {vm_id}")]
    PortNotExposed { vm_id: String, port: u16 },

    #[error("proxy token error: {0}")]
    ProxyToken(String),

    #[error("HOSTD_API_TOKEN environment variable must be set to a non-empty value")]
    MissingApiToken,
    //#[error("{0}")]
    //Other(String),
}

impl Error {
    pub(crate) fn io_other(msg: impl Into<String>) -> Self {
        Error::Io(std::io::Error::other(msg.into()))
    }

    pub(crate) fn vmm(msg: impl Into<String>) -> Self {
        Error::Vmm(msg.into())
    }

    pub(crate) fn net(msg: impl Into<String>) -> Self {
        Error::Net(msg.into())
    }

    pub(crate) fn storage(msg: impl Into<String>) -> Self {
        Error::Storage(msg.into())
    }

    pub(crate) fn proxy_token(msg: impl Into<String>) -> Self {
        Error::ProxyToken(msg.into())
    }
}

impl<T> From<PoisonError<T>> for Error {
    fn from(err: PoisonError<T>) -> Self {
        Error::Lock(err.to_string())
    }
}

pub(crate) type Result<T> = std::result::Result<T, Error>;
