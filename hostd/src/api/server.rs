use axum::{
    Router,
    body::Body,
    extract::State,
    http::{Request, StatusCode},
    middleware::{self, Next},
    response::Response,
    routing::get,
};
use std::sync::Arc;
use tower_http::trace::TraceLayer;

use crate::error::{Error, Result};
use crate::proxy::{ProxyTokens, bearer_token};
use crate::vmm::Vmm;

use super::error::ApiError;
use super::routes::{health::health, vm};

// The environment variable name for the API token.
const API_TOKEN_ENV_VAR: &str = "TIKOVM_HOSTD_API_TOKEN";

/// Shared application state, made available to route handlers.
#[derive(Clone)]
pub(crate) struct AppState {
    pub(crate) vmm: Arc<dyn Vmm>,
    pub(crate) tokens: Arc<ProxyTokens>,
}

pub(crate) struct ApiServer {
    token: Arc<str>,
    vmm: Arc<dyn Vmm>,
    tokens: Arc<ProxyTokens>,
}

impl ApiServer {
    pub(crate) fn new(vmm: Arc<dyn Vmm>, tokens: Arc<ProxyTokens>) -> Result<Self> {
        let token = std::env::var(API_TOKEN_ENV_VAR)
            .ok()
            .filter(|t| !t.is_empty())
            .ok_or(Error::MissingApiToken)?;

        Ok(Self {
            token: token.into(),
            vmm,
            tokens,
        })
    }

    pub(crate) async fn run(&self, addr: &str) -> Result<()> {
        let listener = tokio::net::TcpListener::bind(addr).await?;

        let app = Router::new()
            .nest("/api", self.api_routes())
            .layer(middleware::from_fn_with_state(
                self.token.clone(),
                require_bearer_token,
            ))
            .layer(TraceLayer::new_for_http());

        axum::serve(listener, app).await?;

        Ok(())
    }

    fn api_routes(&self) -> Router {
        let state = AppState {
            vmm: self.vmm.clone(),
            tokens: self.tokens.clone(),
        };

        Router::new()
            .route("/health", get(health))
            .nest("/vms", vm::routes())
            .with_state(state)
    }
}

async fn require_bearer_token(
    State(expected): State<Arc<str>>,
    request: Request<Body>,
    next: Next,
) -> std::result::Result<Response, ApiError> {
    let authorized = bearer_token(request.headers())
        .is_some_and(|token| constant_time_eq(token.as_bytes(), expected.as_bytes()));

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
