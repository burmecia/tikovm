mod api;
mod error;
mod net;
mod proxy;
mod vmm;

use std::fs;
use std::sync::Arc;

use clap::Parser;
use tracing::{error, info};
use tracing_subscriber::{self, EnvFilter};

use crate::{
    api::ApiServer, error::Result, net::NetworkManager, proxy::ProxyTokens,
    vmm::firecracker::FirecrackerVmm,
};

#[derive(Parser, Debug)]
#[command(name = "hostd", about = "Tikovm host daemon")]
struct Args {
    /// Directory containing VM assets (kernel, rootfs, etc.)
    #[arg(long)]
    assets_dir: String,

    /// Directory for runtime state (sockets, logs)
    #[arg(long, default_value = "/tmp/tikovm")]
    work_dir: String,

    /// Address for the API server to listen on
    #[arg(long, default_value = "0.0.0.0:3000")]
    api_listen: String,

    /// Address for the exposed-port proxy server to listen on
    #[arg(long, default_value = "0.0.0.0:8080")]
    proxy_listen: String,

    /// Supernet (CIDR) from which per-project subnets are carved
    #[arg(long, default_value = "172.16.0.0/12")]
    net_supernet: String,

    /// Prefix length of each per-project subnet
    #[arg(long, default_value_t = 24)]
    net_subnet_prefix: u8,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive("info".parse()?))
        .init();

    let args = Args::parse();

    fs::create_dir_all(&args.work_dir)?;

    let net_mgr = NetworkManager::new(&args.work_dir, &args.net_supernet, args.net_subnet_prefix)?;
    net_mgr.reconcile_on_startup()?;

    let fc_vmm = Arc::new(FirecrackerVmm::new(
        &args.assets_dir,
        &args.work_dir,
        Arc::new(net_mgr),
    )?);
    fc_vmm.start_background_tasks();
    let tokens = Arc::new(ProxyTokens::new());
    let api_server = ApiServer::new(fc_vmm.clone(), tokens.clone())?;
    let proxy_server = proxy::ProxyServer::new(fc_vmm, tokens);

    info!(
        api_listen = %args.api_listen,
        proxy_listen = %args.proxy_listen,
        work_dir = %args.work_dir,
        "Tikovm hostd started"
    );

    // Run both servers; the first one to fail shuts down the daemon.
    if let Err(e) = tokio::try_join!(
        api_server.run(&args.api_listen),
        proxy_server.run(&args.proxy_listen),
    ) {
        error!("Tikovm hostd server failed: {}", e);
        return Err(e);
    }

    Ok(())
}
