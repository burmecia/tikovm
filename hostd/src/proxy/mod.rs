//! JWT-authenticated reverse proxy for the exposed ports of VMs.
//!
//! This is the data-plane counterpart of the management API: it listens on
//! its own address (`--proxy-listen`) and forwards connections to
//! `<guest_ip>:<port>`, where the target VM and port come from an ephemeral
//! JWT minted by the management API
//! (`POST /api/vms/{id}/ports/{port}/token`). Both modes re-validate the
//! target against live state on every connection (see `target.rs`), so
//! unexposing a port or destroying the VM revokes access immediately.
//!
//! Two forwarding modes share the one listener; `server.rs` peeks at the
//! first bytes of each accepted connection to pick one:
//!
//! - HTTP (`http.rs`): the JWT rides in the `Authorization: Bearer` header
//!   (`proto: "http"` claims) and the request is forwarded to
//!   `http://<guest_ip>:<port>`.
//! - TCP (`tcp.rs`), for the Postgres wire protocol: the JWT rides in the
//!   `tikovm_token` parameter of the length-prefixed StartupMessage
//!   (`proto: "tcp"` claims — stock libpq can set it via the `options`
//!   connection parameter, e.g. `options='-c tikovm_token=<jwt>'`). Only the
//!   startup phase is touched: SSL/GSS encryption requests get an `N`
//!   (plaintext only; TLS termination is out of scope), the token parameter
//!   is stripped (a stock Postgres would reject the unknown parameter), the
//!   rewritten StartupMessage is forwarded, and both directions are then
//!   spliced with `tokio::io::copy_bidirectional`. Failures are reported as a
//!   Postgres ErrorResponse so psql shows a clean server error.

mod http;
mod server;
mod target;
mod tcp;
mod token;

pub(crate) use server::ProxyServer;
pub(crate) use token::{DEFAULT_TTL_SECS, Proto, ProxyTokens};
