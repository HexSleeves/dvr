pub mod routes;
pub mod state;
pub mod watcher;

use std::path::{Path, PathBuf};

/// `$DVR_STATE_DIR`, else `~/.local/state/dvr`.
pub fn state_dir() -> PathBuf {
    std::env::var_os("DVR_STATE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| dirs::home_dir().expect("cannot determine home directory").join(".local/state/dvr"))
}

pub fn socket_path() -> PathBuf {
    state_dir().join("dvrd.sock")
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

/// Serves on `dir/dvrd.sock` with registry and repos loaded from `dir`. Split
/// from `run()` so tests can point each daemon at its own state dir without
/// mutating the process environment (parallel-safe).
///
/// Singleton per state dir: two daemons rebinding the same socket would both
/// write the same repos' op stores. A live daemon (its socket answers
/// `/health`) makes this fail fast, and an exclusive flock on `dvrd.lock` —
/// held for the daemon's lifetime, released by the kernel on any exit —
/// collapses racing auto-starts (`dvr`'s client spawns dvrd optimistically):
/// the loser exits before touching the winner's socket, and the client's
/// health polling settles on the winner.
pub async fn run_with_dir(dir: PathBuf) -> anyhow::Result<()> {
    std::fs::create_dir_all(&dir)?;
    let sock = dir.join("dvrd.sock");
    if sock.exists() && health_answers(&sock).await {
        anyhow::bail!("dvrd already running (socket {} answers /health)", sock.display());
    }
    let _lock = singleton_lock(&dir)?;
    // Pidfile so out-of-process supervisors (and the CLI e2e tests) can find
    // and stop this daemon. Best-effort: in-process test daemons share a pid.
    let _ = std::fs::write(dir.join("dvrd.pid"), std::process::id().to_string());
    let state = state::DaemonState::load(&dir).await?;
    // Crash-safety: re-scan every registered repo BEFORE serving, so edits
    // made while the daemon was down land in the oplog (spec: error handling).
    state.snapshot_all_repos().await?;

    // Auto-snapshot on file changes: watch every workspace root of every
    // registered repo; roots appearing later (register, workspace-new) are
    // added via DaemonState::watch_root.
    let roots = state.workspace_roots().await;
    state.set_watcher(watcher::spawn(state.clone(), roots)?);

    // Only the lock holder may remove/rebind the socket (a losing racer must
    // never tear down the winner's).
    let _ = std::fs::remove_file(&sock);
    let listener = tokio::net::UnixListener::bind(&sock)?;
    tracing::info!(socket = %sock.display(), "dvrd listening");
    axum::serve(listener, routes::router(state)).await?;
    Ok(())
}

/// Takes the exclusive advisory lock on `dir/dvrd.lock`. The returned handle
/// must stay alive for the daemon's lifetime; flock(2) drops it automatically
/// when the process (or the last handle) dies, so a crashed daemon never
/// blocks the next start. Works in-process too (each `File` is its own open
/// file description), which is what the singleton test exercises.
fn singleton_lock(dir: &Path) -> anyhow::Result<std::fs::File> {
    let path = dir.join("dvrd.lock");
    let file = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(&path)?;
    rustix::fs::flock(&file, rustix::fs::FlockOperation::NonBlockingLockExclusive).map_err(
        |_| anyhow::anyhow!("dvrd already running (could not lock {})", path.display()),
    )?;
    Ok(file)
}

/// True when a live daemon answers `GET /health` on `sock`. A stale socket
/// file (daemon crashed) refuses the connection and reports false.
async fn health_answers(sock: &Path) -> bool {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    let probe = async {
        let mut stream = tokio::net::UnixStream::connect(sock).await.ok()?;
        stream
            .write_all(b"GET /health HTTP/1.1\r\nHost: dvrd\r\nConnection: close\r\n\r\n")
            .await
            .ok()?;
        let mut buf = [0u8; 32];
        let n = stream.read(&mut buf).await.ok()?;
        std::str::from_utf8(&buf[..n]).ok()?.starts_with("HTTP/1.1 200").then_some(())
    };
    tokio::time::timeout(std::time::Duration::from_secs(1), probe).await == Ok(Some(()))
}
