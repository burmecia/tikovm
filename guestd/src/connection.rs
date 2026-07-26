//! Host connection handling: read NDJSON requests and dispatch to the agent.

use std::fs::File;
use std::io::{BufRead, BufReader};
use std::sync::Arc;

use tracing::{info, warn};

use crate::agent::{Agent, ConnWriter};
use crate::proto::Request;

/// Serve one host connection until it drops: parse each line as a request
/// and dispatch it to the agent, then release the connection's writer.
pub(crate) fn handle(reader: File, writer: ConnWriter, agent: Arc<Agent>) {
    for line in BufReader::new(reader).lines() {
        match line {
            Ok(line) => match serde_json::from_str::<Request>(&line) {
                Ok(request) => agent.handle_request(request),
                Err(e) => warn!(error = %e, line, "ignoring malformed request"),
            },
            Err(e) => {
                info!(error = %e, "connection read error");
                break;
            }
        }
    }
    agent.clear_conn(&writer);
    info!("host disconnected");
}
