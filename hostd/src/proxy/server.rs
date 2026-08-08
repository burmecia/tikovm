//! The proxy server: a raw TCP accept loop dispatching per connection to
//! either the HTTP/1.1 handler or the raw-TCP (Postgres) handler.
//!
//! Keeping the accept loop raw (instead of `axum::serve`) is what makes the
//! two-mode dispatch possible: the first bytes of each connection are
//! peeked (not consumed) and checked against the Postgres wire format —
//! see `tcp.rs`.

use std::convert::Infallible;
use std::io;
use std::sync::Arc;

use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::{TokioExecutor, TokioIo};
use tokio::net::{TcpListener, TcpStream};
use tracing::{debug, info};

use crate::error::Result;
use crate::vmm::Vmm;

use super::ProxyTokens;
use super::http::{self, ProxyBody};
use super::tcp;

/// Shared state for all proxied connections.
#[derive(Clone)]
pub(crate) struct ProxyState {
    pub(crate) vmm: Arc<dyn Vmm>,
    pub(crate) tokens: Arc<ProxyTokens>,
    /// Connection-pooled hyper client for upstream (guest) requests.
    pub(crate) client: Client<HttpConnector, ProxyBody>,
}

/// The proxy server: accepts TCP connections and dispatches each to the
/// HTTP/1.1 handler or the raw-TCP (Postgres) handler by peeked prefix.
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
                match sniff_prefix(&stream).await {
                    // Postgres wire protocol: raw-TCP forwarding.
                    Ok(prefix) if tcp::looks_like_postgres(&prefix) => {
                        tcp::handle(state, stream).await;
                    }
                    // Everything else: HTTP/1.1 (the peek consumed nothing,
                    // so hyper sees the full byte stream).
                    Ok(_) => {
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
                    }
                    Err(e) => {
                        debug!(%peer, error = %e, "proxy connection closed before any bytes");
                    }
                }
            });
        }
    }
}

/// Peek (without consuming) until at least 8 bytes — enough for the
/// Postgres frame header — have arrived or the peer goes away.
async fn sniff_prefix(stream: &TcpStream) -> io::Result<[u8; 8]> {
    let mut buf = [0u8; 8];
    loop {
        let n = stream.peek(&mut buf).await?;
        if n == 0 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "peer closed"));
        }
        if n >= buf.len() {
            return Ok(buf);
        }
    }
}
