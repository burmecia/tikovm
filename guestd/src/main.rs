//! guestd — tikovm guest agent.
//!
//! Listens on vsock port 5000 for a control connection from hostd and
//! executes workloads (guest processes) on request, streaming their output
//! and exit status back as newline-delimited JSON events (see proto.rs).

mod agent;
mod connection;
mod proto;
mod vsock;

use std::sync::{Arc, Mutex};
use std::thread;

use tracing::{error, info};

use crate::agent::Agent;
use crate::vsock::VsockListener;

/// Vsock port guestd listens on; hostd connects to it through the VM's
/// Firecracker vsock Unix socket (`CONNECT 5000`).
const VSOCK_PORT: u32 = 5000;

fn main() {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();

    let listener = match VsockListener::bind(VSOCK_PORT) {
        Ok(l) => l,
        Err(e) => {
            error!(error = %e, "failed to bind vsock listener (is CONFIG_VIRTIO_VSOCK enabled?)");
            std::process::exit(1);
        }
    };
    info!(port = VSOCK_PORT, "guestd listening on vsock");

    let agent = Agent::new();

    loop {
        let conn = match listener.accept() {
            Ok(f) => f,
            Err(e) => {
                error!(error = %e, "vsock accept failed");
                std::process::exit(1);
            }
        };
        info!("host connected");
        let writer = match conn.try_clone() {
            Ok(w) => Arc::new(Mutex::new(w)),
            Err(e) => {
                error!(error = %e, "failed to clone connection fd");
                continue;
            }
        };
        agent.set_conn(writer.clone());
        let agent = agent.clone();
        thread::spawn(move || connection::handle(conn, writer, agent));
    }
}
