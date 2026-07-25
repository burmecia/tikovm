mod api;
mod common;
mod error;
mod vmm;

//use clap::Parser;
use tracing_subscriber::{self, EnvFilter};

use crate::{api::ApiServer, error::Result, vmm::firecracker::FirecrackerVmm};
use std::sync::Arc;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive("info".parse()?))
        .init();

    let firecracker_vmm = FirecrackerVmm::new()?;
    let api_server = ApiServer::new(Arc::new(firecracker_vmm))?;

    api_server.run("0.0.0.0:3000").await?;
    Ok(())
}
