pub mod routes;
pub mod state;

use std::path::PathBuf;

/// `$BG_STATE_DIR`, else `~/.local/state/bg`.
pub fn state_dir() -> PathBuf {
    std::env::var_os("BG_STATE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| dirs::home_dir().expect("cannot determine home directory").join(".local/state/bg"))
}

pub fn socket_path() -> PathBuf {
    state_dir().join("bgd.sock")
}

/// Daemon entrypoint: initializes tracing, resolves the state dir from the
/// environment, and serves forever.
pub async fn run() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .try_init()
        .ok();
    run_with_dir(state_dir()).await
}

/// Serves on `dir/bgd.sock` with registry and repos loaded from `dir`. Split
/// from `run()` so tests can point each daemon at its own state dir without
/// mutating the process environment (parallel-safe).
pub async fn run_with_dir(dir: PathBuf) -> anyhow::Result<()> {
    std::fs::create_dir_all(&dir)?;
    let state = state::DaemonState::load(&dir).await?;
    // Crash-safety: re-scan every registered repo BEFORE serving, so edits
    // made while the daemon was down land in the oplog (spec: error handling).
    state.snapshot_all_repos().await;

    let sock = dir.join("bgd.sock");
    let _ = std::fs::remove_file(&sock);
    let listener = tokio::net::UnixListener::bind(&sock)?;
    tracing::info!(socket = %sock.display(), "bgd listening");
    axum::serve(listener, routes::router(state)).await?;
    Ok(())
}
