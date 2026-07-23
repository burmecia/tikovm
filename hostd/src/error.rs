#[derive(Debug, thiserror::Error)]
pub(crate) enum Error {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("tracing_subscriber filter parse: {0}")]
    TracingSubscriberFilterParse(#[from] tracing_subscriber::filter::ParseError),

    #[error("HOSTD_API_TOKEN environment variable must be set to a non-empty value")]
    MissingApiToken,
    //#[error("{0}")]
    //Other(String),
}

pub(crate) type Result<T> = std::result::Result<T, Error>;
