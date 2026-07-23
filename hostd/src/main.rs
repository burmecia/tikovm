mod api;
mod error;

//use clap::Parser;
use tracing_subscriber::{self, EnvFilter};

use crate::{api::ApiServer, error::Result};

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::from_default_env()
                .add_directive("info".parse()?)
                .add_directive("tower_http=debug".parse()?),
        )
        .init();

    let api_server = ApiServer::new()?;

    api_server.run("0.0.0.0:3000").await?;
    Ok(())
}
