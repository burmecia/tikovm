//! The proxy server: a raw TCP accept loop serving HTTP/1.1 per connection.
//!
//! Keeping the accept loop raw (instead of `axum::serve`) is deliberate — it
//! is the seam where raw-TCP proxying (e.g. Postgres) slots in later; see the
//! `proxy` module docs.

use std::convert::Infallible;
use std::sync::Arc;

use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::{TokioExecutor, TokioIo};
use tokio::net::TcpListener;
use tracing::{debug, info};

use crate::error::Result;
use crate::vmm::Vmm;

use super::ProxyTokens;
use super::http::{self, ProxyBody};

/// Shared state for all proxied connections.
#[derive(Clone)]
pub(crate) struct ProxyState {
    pub(crate) vmm: Arc<dyn Vmm>,
    pub(crate) tokens: Arc<ProxyTokens>,
    /// Connection-pooled hyper client for upstream (guest) requests.
    pub(crate) client: Client<HttpConnector, ProxyBody>,
}

/// The proxy server: accepts TCP connections and serves HTTP/1.1 on them.
pub(crate) struct ProxyServer {
    vmm: Arc<dyn Vmm>,
    tokens: Arc<ProxyTokens>,
}

impl ProxyServer {
    pub(crate) fn new(vmm: Arc<dyn Vmm>, tokens: Arc<ProxyTokens>) -> Self {
        Self { vmm, tokens }
    }

    pub(crate) async fn run(&self, addr: &str) -> Result<()> {
        let listener = TcpListener::bind(addr).await?;
        info!(addr = %addr, "Tikovm proxy server listening");

        let state = ProxyState {
            vmm: self.vmm.clone(),
            tokens: self.tokens.clone(),
            client: Client::builder(TokioExecutor::new()).build(HttpConnector::new()),
        };

        loop {
            let (stream, peer) = listener.accept().await?;
            let state = state.clone();
            tokio::spawn(async move {
                let service = service_fn(move |req| {
                    let state = state.clone();
                    async move { Ok::<_, Infallible>(http::handle(state, req).await) }
                });
                if let Err(e) = http1::Builder::new()
                    .serve_connection(TokioIo::new(stream), service)
                    .await
                {
                    debug!(%peer, error = %e, "proxy connection ended with error");
                }
            });
        }
    }
}
