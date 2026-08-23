//! `bgd` — the bg daemon, foreground. Logs go wherever stdout/stderr point;
//! when auto-started by `bg`, that is `state_dir()/bgd.log`.

use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt().with_env_filter(EnvFilter::from_default_env()).init();
    bg_daemon::run().await
}
