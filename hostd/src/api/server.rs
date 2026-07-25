use axum::{
    Router,
    body::Body,
    extract::State,
    http::{Request, StatusCode, header::AUTHORIZATION},
    middleware::{self, Next},
    response::Response,
    routing::get,
};
use std::sync::Arc;
use tower_http::trace::TraceLayer;

use crate::error::{Error, Result};
use crate::vmm::Vmm;

use super::error::ApiError;
use super::routes::{health::health, vm};

// The environment variable name for the API token.
const API_TOKEN_ENV_VAR: &str = "TIKOVM_HOSTD_API_TOKEN";

#[derive(Clone)]
struct AuthState {
    token: Arc<str>,
}

/// Shared application state, made available to route handlers.
/// Add further shared state fields here as needed.
#[derive(Clone)]
pub(crate) struct AppState {
    pub(crate) vmm: Arc<dyn Vmm>,
}

pub(crate) struct ApiServer {
    token: Arc<str>,
    vmm: Arc<dyn Vmm>,
}

impl ApiServer {
    pub(crate) fn new(vmm: Arc<dyn Vmm>) -> Result<Self> {
        let token = std::env::var(API_TOKEN_ENV_VAR)
            .ok()
            .filter(|t| !t.is_empty())
            .ok_or(Error::MissingApiToken)?;

        Ok(Self {
            token: token.into(),
            vmm,
        })
    }

    pub(crate) async fn run(&self, addr: &str) -> Result<()> {
        let listener = tokio::net::TcpListener::bind(addr).await?;

        let auth_state = AuthState {
            token: self.token.clone(),
        };

        let app = Router::new()
            .nest("/api", self.api_routes())
            .layer(middleware::from_fn_with_state(
                auth_state,
                require_bearer_token,
            ))
            .layer(TraceLayer::new_for_http());

        axum::serve(listener, app).await?;

        Ok(())
    }

    fn api_routes(&self) -> Router {
        let state = AppState {
            vmm: self.vmm.clone(),
        };

        Router::new()
            .route("/health", get(health))
            .nest("/vms", vm::routes())
            .with_state(state)
    }
}

async fn require_bearer_token(
    State(state): State<AuthState>,
    request: Request<Body>,
    next: Next,
) -> std::result::Result<Response, ApiError> {
    let authorized = request
        .headers()
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .is_some_and(|token| constant_time_eq(token.as_bytes(), state.token.as_bytes()));

    if !authorized {
        return Err(ApiError::new(
            StatusCode::UNAUTHORIZED,
            "missing or invalid bearer token",
        ));
    }

    Ok(next.run(request).await)
}

/// Compares two byte slices in constant time to avoid leaking token length/content via timing.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |diff, (x, y)| diff | (x ^ y)) == 0
}
