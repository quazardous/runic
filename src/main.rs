mod config;
mod server;
mod upstream;

use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;
use tracing_subscriber::EnvFilter;

#[derive(Parser, Debug)]
#[command(
    name = "runic",
    version,
    about = "Local SOCKS5 proxy relaying via HTTP CONNECT upstream"
)]
struct Cli {
    #[arg(short, long, default_value = "/etc/runic/runic.yaml")]
    config: PathBuf,

    #[arg(long, env = "RUNIC_LOG", default_value = "runic=info,fast_socks5=warn")]
    log: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_new(&cli.log)?)
        .with_target(true)
        .init();

    let cfg = config::Config::load(&cli.config)?;
    server::run(cfg).await
}
