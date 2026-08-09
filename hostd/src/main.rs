mod api;
mod error;
mod net;
mod proxy;
mod storage;
mod vmm;

use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

use clap::{Parser, Subcommand};
use tracing::{error, info, warn};
use tracing_subscriber::{self, EnvFilter};

use crate::{
    api::ApiServer,
    error::{Error, Result},
    net::NetworkManager,
    proxy::ProxyTokens,
    storage::StorageManager,
    vmm::firecracker::FirecrackerVmm,
};

#[derive(Subcommand, Debug)]
enum Commands {
    /// Internal: serve one chunk-backed ublk device for a VM volume.
    /// Spawned by StorageManager; not a user-facing command.
    #[command(name = "ublk-worker", hide = true)]
    UblkWorker {
        /// Volume directory (holds meta.json + chunks/)
        #[arg(long)]
        dir: PathBuf,
        /// Fresh volume size in MiB (omit with --recover)
        #[arg(long)]
        size_mb: Option<u64>,
        /// Chunk size in KiB (default 1024)
        #[arg(long)]
        chunk_kb: Option<u32>,
        /// Reattach to the device left behind by a dead worker
        #[arg(long)]
        recover: bool,
        /// Device id (-1 = auto-allocate; --recover needs a concrete id)
        #[arg(short = 'n', default_value_t = -1, allow_hyphen_values = true)]
        dev_id: i32,
        /// Number of ublk queues
        #[arg(long, default_value_t = 8)]
        queues: u16,
        /// Queue depth (max in-flight IOs per queue)
        #[arg(long, default_value_t = 64)]
        depth: u16,
    },
}

#[derive(Parser, Debug)]
#[command(name = "hostd", about = "Tikovm host daemon")]
struct Args {
    #[command(subcommand)]
    command: Option<Commands>,

    /// Directory containing VM assets (kernel, rootfs, etc.). Required in
    /// server mode (the `ublk-worker` subcommand does not need it).
    #[arg(long)]
    assets_dir: Option<String>,

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

    /// Root directory for per-VM block-storage volumes (chunk files).
    /// Production: an S3 Files mount.
    #[arg(long, default_value = "/mnt/s3files/vm_storage")]
    storage_root: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    // Worker subprocess mode: serve one ublk device, then exit. Runs on
    // the tokio runtime only nominally — ublk_worker::run blocks on its
    // own queue threads. Tracing goes to stderr here: stdout is reserved
    // for the "device id: N" handshake the parent parses (libublk logs to
    // stdout with the default writer and would corrupt the handshake).
    if let Some(Commands::UblkWorker { dir, size_mb, chunk_kb, recover, dev_id, queues, depth }) =
        args.command
    {
        tracing_subscriber::fmt()
            .with_writer(std::io::stderr)
            .with_env_filter(EnvFilter::from_default_env().add_directive("info".parse()?))
            .init();
        return storage::ublk_worker::run(storage::ublk_worker::WorkerArgs {
            dir,
            size_mb,
            chunk_kb,
            recover,
            dev_id,
            queues,
            depth,
        });
    }

    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive("info".parse()?))
        .init();

    fs::create_dir_all(&args.work_dir)?;

    let assets_dir = args
        .assets_dir
        .ok_or_else(|| Error::vmm("--assets-dir is required in server mode"))?;

    let net_mgr = NetworkManager::new(&args.work_dir, &args.net_supernet, args.net_subnet_prefix)?;
    net_mgr.reconcile_on_startup()?;

    let storage_mgr = Arc::new(StorageManager::new(&args.storage_root));
    if !std::path::Path::new(&args.storage_root).is_dir() {
        warn!(
            storage_root = %args.storage_root,
            "storage root is not a directory; VMs with block_storage will fail at create time"
        );
    } else {
        storage_mgr.reconcile_on_startup();
    }

    let fc_vmm = Arc::new(FirecrackerVmm::new(
        &assets_dir,
        &args.work_dir,
        Arc::new(net_mgr),
        storage_mgr,
    ));
    fc_vmm.start_background_tasks();
    let tokens = Arc::new(ProxyTokens::new());
    let api_server = ApiServer::new(fc_vmm.clone(), tokens.clone())?;
    let proxy_server = proxy::ProxyServer::new(fc_vmm, tokens);

    info!(
        api_listen = %args.api_listen,
        proxy_listen = %args.proxy_listen,
        work_dir = %args.work_dir,
        storage_root = %args.storage_root,
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
