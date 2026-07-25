mod api;
mod common;
mod error;
mod vmm;

use std::fs;
use std::sync::Arc;

use clap::Parser;
use tracing::{debug, error, info};
use tracing_subscriber::{self, EnvFilter};

use crate::{api::ApiServer, error::Result, vmm::firecracker::FirecrackerVmm};

#[derive(Parser, Debug)]
#[command(name = "hostd", about = "Tikovm host daemon")]
struct Args {
    /// Directory containing VM assets (kernel, rootfs, etc.)
    #[arg(long)]
    assets_dir: String,

    /// Directory for runtime state (sockets, logs)
    #[arg(long, default_value = "/tmp/tikovm")]
    run_dir: String,

    /// Address for the API server to listen on
    #[arg(long, default_value = "0.0.0.0:3000")]
    api_listen: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive("info".parse()?))
        .init();

    let args = Args::parse();

    fs::create_dir_all(&args.run_dir)?;

    let fc_vmm = FirecrackerVmm::new(&args.assets_dir, &args.run_dir)?;
    let api_server = ApiServer::new(Arc::new(fc_vmm))?;

    let addr = args.api_listen.as_str();

    match api_server.run(addr).await {
        Ok(_) => {
            info!(addr = %addr, run_dir = %args.run_dir, "Tikovm hostd started");
        }
        Err(e) => {
            error!("Failed to start Tikovm hostd server: {}", e);
            return Err(e);
        }
    }

    Ok(())
}
