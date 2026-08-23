//! `dvrd` — the dvr daemon, foreground. Logs go wherever stdout/stderr point;
//! when auto-started by `dvr`, that is `state_dir()/dvrd.log`.

use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();
    dvr_daemon::run().await
}
