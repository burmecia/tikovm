use axum::{
    Json,
    extract::{FromRequest, Request, rejection::JsonRejection},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde_json::json;
use std::sync::PoisonError;

use crate::error::Error;

/// Result type for API route handlers.
pub(crate) type ApiResult<T> = std::result::Result<T, ApiError>;

/// Error returned by API route handlers. Every failure is serialized to the
/// same JSON body: `{ "error": { "code": <http status>, "message": "..." } }`.
pub(crate) struct ApiError {
    status: StatusCode,
    message: String,
}

impl ApiError {
    pub(crate) fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }

    pub(crate) fn internal(message: impl Into<String>) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, message)
    }
}

impl From<Error> for ApiError {
    fn from(err: Error) -> Self {
        match err {
            Error::VmNotFound(_) | Error::WorkloadNotFound(_) | Error::PortNotExposed { .. } => {
                Self::new(StatusCode::NOT_FOUND, err.to_string())
            }
            Error::PortAlreadyExposed { .. } | Error::InvalidStateTransition { .. } => {
                Self::new(StatusCode::CONFLICT, err.to_string())
            }
            Error::InvalidPort(_) | Error::InvalidImage(_) => {
                Self::new(StatusCode::BAD_REQUEST, err.to_string())
            }
            // TODO: map other error variants to more specific status codes.
            _ => Self::internal(err.to_string()),
        }
    }
}

impl From<JsonRejection> for ApiError {
    fn from(rejection: JsonRejection) -> Self {
        Self::new(rejection.status(), rejection.body_text())
    }
}

impl<T> From<PoisonError<T>> for ApiError {
    fn from(err: PoisonError<T>) -> Self {
        ApiError::internal(err.to_string())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(json!({
                "error": {
                    "code": self.status.as_u16(),
                    "message": self.message,
                }
            })),
        )
            .into_response()
    }
}

/// JSON body extractor that converts extraction failures into [`ApiError`],
/// so malformed request bodies also produce the uniform error format.
pub(crate) struct ApiJson<T>(pub T);

impl<S, T> FromRequest<S> for ApiJson<T>
where
    Json<T>: FromRequest<S, Rejection = JsonRejection>,
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        let Json(value) = Json::<T>::from_request(req, state).await?;
        Ok(Self(value))
    }
}
