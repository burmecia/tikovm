//! `vmtop` — a top/htop-style terminal monitor for tikovm hostd.
//!
//! Talks to the hostd REST API (Bearer-token authenticated) and periodically
//! renders the full VM inventory grouped by project. Read-only.

mod api;
mod app;
mod error;
mod format;
mod model;
mod ui;
mod view;

use std::time::Duration;

use clap::Parser;
use tokio::sync::{mpsc, watch};

use crate::app::{App, Snapshot};
use crate::error::{Error, Result};
use crate::view::SortOrder;

/// Command-line arguments, mirroring the house clap-derive style.
#[derive(Debug, Parser)]
#[command(name = "vmtop", about = "top/htop-style VM monitor for tikovm hostd")]
struct Args {
    /// Base URL of the hostd API (scheme and port).
    #[arg(long, default_value = "http://127.0.0.1:3000")]
    api_url: String,

    /// Bearer token; optional if TIKOVM_HOSTD_API_TOKEN is set.
    #[arg(long)]
    token: Option<String>,

    /// VM list poll/refresh interval in milliseconds.
    #[arg(short = 'r', long, default_value_t = 1000)]
    refresh_ms: u64,

    /// Start in flat table mode instead of grouping by project.
    #[arg(short = 'f', long)]
    flat: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    init_logging();

    let token = args
        .token
        .or_else(|| std::env::var("TIKOVM_HOSTD_API_TOKEN").ok())
        .filter(|t| !t.trim().is_empty())
        .ok_or(Error::MissingToken)?;

    let interval = Duration::from_millis(args.refresh_ms.clamp(100, 30_000));
    let client = api::ApiClient::new(args.api_url.clone(), token)?;

    let (tx, rx) = watch::channel(Snapshot::fresh());
    let (trigger, trigger_rx) = mpsc::unbounded_channel::<()>();
    let poll_interval = interval;
    tokio::spawn(async move {
        app::poll_loop(client, poll_interval, trigger_rx, tx).await;
    });

    let mut terminal = app::init_terminal()?;
    let mut tui = App::new(
        args.api_url.clone(),
        interval,
        trigger,
        rx,
        !args.flat,
        SortOrder::State,
    );
    let run = tui.run(&mut terminal);
    // Restore the terminal first — even if the loop errored — so the user is
    // dropped back into a usable shell.
    let rest = app::restore_terminal();
    run.and(rest)
}

/// House-style tracing to stderr with an env-filter, `info` default.
fn init_logging() {
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("vmtop=info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();
}
