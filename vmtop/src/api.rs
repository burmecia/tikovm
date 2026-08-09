//! Thin HTTP client for the hostd REST API.
//!
//! `vmtop` only needs read access, so this wraps a single `GET /api/vms`
//! call (which returns the full `VmInstance` inventory) behind a reqwest
//! client configured with a bounded request timeout so the poller never
//! stalls even if hostd hangs.

use serde::de::DeserializeOwned;

use crate::error::{Error, Result};
use crate::model::Vm;

/// Client for the subset of the hostd API `vmtop` consumes.
#[derive(Clone)]
pub(crate) struct ApiClient {
    http: reqwest::Client,
    base_url: String,
    token: String,
}

impl ApiClient {
    /// Create a client pointing at the hostd API (e.g. `http://127.0.0.1:3000`).
    pub(crate) fn new(base_url: impl Into<String>, token: impl Into<String>) -> Result<Self> {
        let http = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(3))
            .timeout(std::time::Duration::from_secs(5))
            .build()
            .map_err(|e| Error::Api(e.to_string()))?;
        Ok(Self {
            http,
            base_url: base_url.into(),
            token: token.into(),
        })
    }

    /// Fetch the full VM inventory (`GET /api/vms`). One call is one poll.
    pub(crate) async fn list_vms(&self) -> Result<Vec<Vm>> {
        self.get("/api/vms").await
    }

    async fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T> {
        let url = format!("{}{}", self.base_url.trim_end_matches('/'), path);
        let resp = self
            .http
            .get(url)
            .header(
                reqwest::header::AUTHORIZATION,
                format!("Bearer {}", self.token),
            )
            .send()
            .await
            .map_err(|e| Error::Api(e.to_string()))?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(Error::Http {
                status: status.as_u16(),
                body,
            });
        }
        resp.json::<T>()
            .await
            .map_err(|e| Error::Api(e.to_string()))
    }
}
