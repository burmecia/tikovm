mod api;
mod common;
mod error;
mod vmm;

use std::fs;
use std::sync::Arc;

//use clap::Parser;
use tracing::{debug, error, info};
use tracing_subscriber::{self, EnvFilter};

use crate::{api::ApiServer, error::Result, vmm::firecracker::FirecrackerVmm};

const RUN_DIR: &str = "/tmp/tikovm";

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive("info".parse()?))
        .init();

    fs::create_dir_all(RUN_DIR)?;

    let fc_vmm = FirecrackerVmm::new(RUN_DIR)?;
    let api_server = ApiServer::new(Arc::new(fc_vmm))?;

    let addr = "0.0.0.0:3000";

    match api_server.run(addr).await {
        Ok(_) => {
            info!(addr = %addr, run_dir = %RUN_DIR, "Tikovm hostd started");
        }
        Err(e) => {
            error!("Failed to start Tikovm hostd server: {}", e);
            return Err(e);
        }
    }

    Ok(())
}
