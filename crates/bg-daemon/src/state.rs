use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use bg_engine::RepoEngine;
use bg_proto::{RepoId, RepoInfo};
use tokio::sync::{Mutex, RwLock};

/// Shared daemon state: the registry of repos, each wrapped in an async mutex
/// so at most one engine operation runs per repo at a time.
#[derive(Clone)]
pub struct DaemonState {
    inner: Arc<Inner>,
}

struct Inner {
    state_dir: PathBuf,
    /// (name, email) used for jj commits, from `git config` at load time.
    identity: (String, String),
    repos: RwLock<HashMap<String, RepoHandle>>,
    /// The filesystem watcher, installed by `run_with_dir` after the startup
    /// re-scan (`None` until then, and in tests that never start one).
    /// Dropping the handle stops watching, so it lives here.
    watcher: std::sync::Mutex<Option<crate::watcher::WatcherHandle>>,
}

#[derive(Clone)]
struct RepoHandle {
    info: RepoInfo,
    engine: Arc<Mutex<RepoEngine>>,
}

/// Register failures, split so routes can map them to HTTP statuses without
/// inspecting message strings.
#[derive(Debug, thiserror::Error)]
pub enum RegisterError {
    /// The request itself is bad (nonexistent path, not a git repo, ...).
    #[error("{0}")]
    Invalid(String),
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

impl DaemonState {
    /// Reads `repos.json` from `dir` and opens an engine for every surviving
    /// entry. Repos whose root vanished (or no longer opens) are dropped with
    /// a warning, and the pruned registry is persisted back.
    pub async fn load(dir: &Path) -> anyhow::Result<Self> {
        let identity = git_identity();
        let mut repos = HashMap::new();
        let registry_path = dir.join("repos.json");
        let mut dropped_any = false;
        if registry_path.exists() {
            let infos: Vec<RepoInfo> = serde_json::from_slice(&std::fs::read(&registry_path)?)?;
            for info in infos {
                if !info.root.is_dir() {
                    tracing::warn!(id = %info.id.0, root = %info.root.display(),
                        "dropping registered repo: path vanished");
                    dropped_any = true;
                    continue;
                }
                let settings = bg_engine::settings::make_settings(&identity.0, &identity.1)?;
                let root = info.root.clone();
                let open = run_engine(move || async move {
                    RepoEngine::open_or_init(&root, settings).await
                })
                .await;
                match open {
                    Ok(engine) => {
                        repos.insert(
                            info.id.0.clone(),
                            RepoHandle { info, engine: Arc::new(Mutex::new(engine)) },
                        );
                    }
                    Err(err) => {
                        tracing::warn!(id = %info.id.0, root = %info.root.display(),
                            "dropping registered repo: failed to open: {err:#}");
                        dropped_any = true;
                    }
                }
            }
        }

        let state = Self {
            inner: Arc::new(Inner {
                state_dir: dir.to_path_buf(),
                identity,
                repos: RwLock::new(repos),
                watcher: std::sync::Mutex::new(None),
            }),
        };
        if dropped_any {
            state.persist(&*state.inner.repos.read().await)?;
        }
        Ok(state)
    }

    /// Registers the repo at `path` (idempotent per canonical root). The id is
    /// the directory name, deduplicated with `-2`/`-3`/... suffixes.
    pub async fn register(&self, path: &Path) -> Result<RepoInfo, RegisterError> {
        let root = path
            .canonicalize()
            .map_err(|err| RegisterError::Invalid(format!("path {}: {err}", path.display())))?;

        let mut repos = self.inner.repos.write().await;
        if let Some(h) = repos.values().find(|h| h.info.root == root) {
            return Ok(h.info.clone());
        }

        let name = root
            .file_name()
            .and_then(|n| n.to_str())
            .map(str::to_string)
            .ok_or_else(|| {
                RegisterError::Invalid(format!("cannot derive a repo id from {}", root.display()))
            })?;
        let mut id = name.clone();
        let mut n = 2;
        while repos.contains_key(&id) {
            id = format!("{name}-{n}");
            n += 1;
        }

        let settings = bg_engine::settings::make_settings(&self.inner.identity.0, &self.inner.identity.1)
            .map_err(RegisterError::Internal)?;
        // Any open failure at register time means the caller pointed us at
        // something that isn't a usable git repo -> invalid request.
        let open_root = root.clone();
        let engine = run_engine(move || async move {
            RepoEngine::open_or_init(&open_root, settings).await
        })
        .await
        .map_err(|err| RegisterError::Invalid(format!("{err:#}")))?;

        let info = RepoInfo { id: RepoId(id.clone()), root };
        repos.insert(id.clone(), RepoHandle { info: info.clone(), engine: Arc::new(Mutex::new(engine)) });
        if let Err(err) = self.persist(&repos) {
            repos.remove(&id);
            return Err(RegisterError::Internal(err));
        }

        // Auto-snapshot the new repo on file changes.
        self.watch_root(info.id.clone(), info.root.clone());
        Ok(info)
    }

    /// Starts watching one root — a repo root at register time, or a new
    /// workspace dir at ws-new time. A watch failure loses auto-snapshot only
    /// (explicit API calls still work), so it degrades to a warning.
    pub fn watch_root(&self, id: RepoId, root: PathBuf) {
        if let Some(watcher) = self.inner.watcher.lock().unwrap().as_ref()
            && let Err(err) = watcher.watch_path(id.clone(), root.clone())
        {
            tracing::warn!(id = %id.0, root = %root.display(), "failed to watch root: {err:#}");
        }
    }

    /// Every root the watcher should cover: for each repo, the workspace
    /// roots its engine knows about (the default workspace root is the repo
    /// root). Used to seed the watcher at startup.
    pub async fn workspace_roots(&self) -> Vec<(RepoId, PathBuf)> {
        let handles: Vec<RepoHandle> = self.inner.repos.read().await.values().cloned().collect();
        let mut roots = Vec::new();
        for h in handles {
            // Sync accessor: list_workspaces awaits no jj future, so locking
            // on this task is fine (no !Send future crosses a thread).
            let engine = h.engine.lock().await;
            for ws in engine.list_workspaces() {
                roots.push((h.info.id.clone(), ws.path));
            }
        }
        roots
    }

    /// Installs the filesystem watcher handle (see `watcher::spawn`), keeping
    /// it alive for the daemon's lifetime.
    pub fn set_watcher(&self, watcher: crate::watcher::WatcherHandle) {
        *self.inner.watcher.lock().unwrap() = Some(watcher);
    }

    /// Looks up a repo by id, or by any absolute path inside a registered
    /// root (the CLI sends its cwd).
    pub async fn resolve(&self, id_or_path: &str) -> Option<(RepoInfo, Arc<Mutex<RepoEngine>>)> {
        let repos = self.inner.repos.read().await;
        if let Some(h) = repos.get(id_or_path) {
            return Some((h.info.clone(), h.engine.clone()));
        }
        let path = Path::new(id_or_path);
        if !path.is_absolute() {
            return None;
        }
        let canon = path.canonicalize().ok()?;
        repos
            .values()
            .filter(|h| canon.starts_with(&h.info.root))
            // Nested registered roots: the deepest containing root wins.
            .max_by_key(|h| h.info.root.components().count())
            .map(|h| (h.info.clone(), h.engine.clone()))
    }

    /// All registrations, sorted by id.
    pub async fn list(&self) -> Vec<RepoInfo> {
        let repos = self.inner.repos.read().await;
        let mut infos: Vec<RepoInfo> = repos.values().map(|h| h.info.clone()).collect();
        infos.sort_by(|a, b| a.id.0.cmp(&b.id.0));
        infos
    }

    /// Snapshots every registered repo. Failures are logged, not fatal — one
    /// broken repo must not take the daemon down.
    pub async fn snapshot_all_repos(&self) {
        let handles: Vec<RepoHandle> = self.inner.repos.read().await.values().cloned().collect();
        for h in handles {
            let engine = h.engine.clone();
            let result = run_engine(move || async move {
                engine.lock().await.snapshot_all().await.map(|_| ())
            })
            .await;
            if let Err(err) = result {
                tracing::warn!(id = %h.info.id.0, "snapshot failed: {err:#}");
            }
        }
    }

    /// Atomically rewrites `repos.json` (write tmp + rename).
    fn persist(&self, repos: &HashMap<String, RepoHandle>) -> anyhow::Result<()> {
        let mut infos: Vec<&RepoInfo> = repos.values().map(|h| &h.info).collect();
        infos.sort_by(|a, b| a.id.0.cmp(&b.id.0));
        let path = self.inner.state_dir.join("repos.json");
        let tmp = self.inner.state_dir.join("repos.json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(&infos)?)?;
        std::fs::rename(&tmp, &path)?;
        Ok(())
    }
}

/// The user's git identity for jj commits, with a fallback so the daemon
/// works on machines without a configured git identity.
fn git_identity() -> (String, String) {
    let get = |key: &str| -> Option<String> {
        let out = Command::new("git").args(["config", key]).output().ok()?;
        if !out.status.success() {
            return None;
        }
        let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
        (!s.is_empty()).then_some(s)
    };
    (
        get("user.name").unwrap_or_else(|| "bg".to_string()),
        get("user.email").unwrap_or_else(|| "bg@localhost".to_string()),
    )
}

/// Runs an engine future to completion on a blocking thread.
///
/// jj-lib 0.44's async fns return `!Send` futures (`Box<dyn MutableIndex>`,
/// `dyn OpHeadsStoreLock`, `dyn Revset`, ... are held across awaits), so they
/// cannot be awaited on axum's multi-threaded handler tasks. The closure runs
/// on a `spawn_blocking` thread and builds its future there, so the future
/// never crosses a thread. jj-lib's futures don't need a reactor (its only
/// tokio use is behind the `watchman` feature), so a plain `futures` executor
/// suffices — and it avoids nested-tokio-runtime traps.
pub(crate) async fn run_engine<T, F, Fut>(f: F) -> anyhow::Result<T>
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: Future<Output = anyhow::Result<T>>,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(move || futures::executor::block_on(f()))
        .await
        .map_err(|err| anyhow::anyhow!("engine task panicked: {err}"))?
}
