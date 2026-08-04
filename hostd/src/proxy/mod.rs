//! JWT-authenticated reverse proxy for the exposed ports of VMs.
//!
//! This is the data-plane counterpart of the management API: it listens on
//! its own address (`--proxy-listen`) and forwards requests to
//! `http://<guest_ip>:<port>`, where the target VM and port come from a
//! per-request bearer JWT minted by the management API
//! (`POST /api/vms/{id}/ports/{port}/token`). See `http.rs` for the request
//! path and `token.rs` for the token model.
//!
//! Only HTTP is forwarded today, but the raw `TcpListener` accept loop in
//! `server.rs` is the deliberate seam for raw-TCP proxying (e.g. Postgres): a
//! TCP mode reads the protocol's own handshake — for Postgres the
//! length-prefixed StartupMessage (first 4 bytes = frame length, so exactly
//! one frame is read) — extracts the JWT from an agreed startup parameter,
//! validates it once (`proto: "tcp"` claims), strips/rewrites the token field
//! (a stock Postgres would reject an unknown startup parameter), forwards the
//! handshake to the guest, and then splices both directions with
//! `tokio::io::copy_bidirectional`. Nothing beyond the first packet needs
//! parsing, and the HTTP path is untouched.

mod http;
mod server;
mod token;

pub(crate) use server::ProxyServer;
pub(crate) use token::{DEFAULT_TTL_SECS, ProxyTokens};
