//! Filesystem watcher: auto-snapshots registered repos when their files
//! change, so the oplog stays current without any client asking for it.
//!
//! One `notify-debouncer-full` debouncer (500ms) watches every registered
//! workspace root recursively. Debounced event batches are mapped back to the
//! repos that own the paths (events under `.jj/` or `.git/` are ignored —
//! snapshotting writes to `.jj/` itself, so this also breaks the feedback
//! loop), deduplicated, and each dirty repo gets one `snapshot_all()`.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use bg_proto::RepoId;
use notify_debouncer_full::notify::{RecommendedWatcher, RecursiveMode};
use notify_debouncer_full::{DebounceEventResult, Debouncer, RecommendedCache, new_debouncer};

use crate::state::{DaemonState, run_engine};

/// The list of watched roots, shared between the event task (which maps event
/// paths to repo ids) and `watch_path` (which grows it on registration).
type Roots = Arc<RwLock<Vec<(RepoId, PathBuf)>>>;

/// Owns the debouncer — dropping this stops all watching, so `DaemonState`
/// holds it for the daemon's lifetime. The `std::sync::Mutex` is interior
/// mutability for `watch_path` (`Debouncer::watch` needs `&mut`); calls are
/// rare (one per registration) and never held across an await.
pub struct WatcherHandle {
    debouncer: Mutex<Debouncer<RecommendedWatcher, RecommendedCache>>,
    roots: Roots,
}

impl WatcherHandle {
    /// Starts watching a newly registered repo root recursively.
    pub fn watch_path(&self, id: RepoId, root: PathBuf) -> anyhow::Result<()> {
        self.debouncer.lock().unwrap().watch(&root, RecursiveMode::Recursive)?;
        self.roots.write().unwrap().push((id, root));
        Ok(())
    }
}

/// Builds the debouncer, watches every root in `initial`, and spawns the
/// event-draining task. The returned handle must be kept alive (store it via
/// `DaemonState::set_watcher`).
pub fn spawn(
    state: DaemonState,
    initial: Vec<(RepoId, PathBuf)>,
) -> anyhow::Result<WatcherHandle> {
    // The debouncer delivers events on its own thread; forward them into the
    // tokio world through an unbounded channel (send never blocks that thread).
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let mut debouncer =
        new_debouncer(Duration::from_millis(500), None, move |res: DebounceEventResult| {
            let _ = tx.send(res);
        })?;
    for (_, root) in &initial {
        debouncer.watch(root, RecursiveMode::Recursive)?;
    }
    let roots: Roots = Arc::new(RwLock::new(initial));

    // Weak, not strong: the state owns this watcher, so a strong clone here
    // would be a cycle that keeps a dead daemon's watcher snapshotting forever
    // (see `WeakDaemonState`). When the daemon drops, the debouncer (and its
    // callback holding `tx`) drops too, `recv` returns `None`, and this task
    // exits.
    let weak = state.downgrade();
    let task_roots = roots.clone();
    tokio::spawn(async move {
        while let Some(res) = rx.recv().await {
            let events = match res {
                Ok(events) => events,
                Err(errors) => {
                    for err in errors {
                        tracing::warn!("watcher error: {err}");
                    }
                    continue;
                }
            };

            // Map event paths to repo ids, deduped per debounce batch. Scoped
            // so the roots read guard drops before any await.
            let mut dirty = HashSet::new();
            {
                let roots = task_roots.read().unwrap();
                for event in &events {
                    for path in &event.paths {
                        let internal = path.components().any(|c| {
                            let s = c.as_os_str();
                            s == ".jj" || s == ".git"
                        });
                        if internal {
                            continue;
                        }
                        // Nested registered roots: deepest containing root
                        // wins (mirrors DaemonState::resolve).
                        if let Some((id, _)) = roots
                            .iter()
                            .filter(|(_, root)| path.starts_with(root))
                            .max_by_key(|(_, root)| root.components().count())
                        {
                            dirty.insert(id.0.clone());
                        }
                    }
                }
            }

            // Upgrade per batch and drop the strong handle before the next
            // `recv` — an in-flight batch racing daemon shutdown is skipped.
            let Some(state) = weak.upgrade() else { break };
            for id in dirty {
                snapshot_repo(&state, &id).await;
            }
        }
    });

    Ok(WatcherHandle { debouncer: Mutex::new(debouncer), roots })
}

/// Snapshots one repo through `run_engine` (jj-lib futures are `!Send` — see
/// its doc comment). Failures are logged, never fatal to the watcher.
async fn snapshot_repo(state: &DaemonState, id: &str) {
    // The repo may have been dropped since the event fired; nothing to do.
    let Some((info, engine)) = state.resolve(id).await else { return };
    let result = run_engine(move || async move {
        engine.lock().await.snapshot_all().await.map(|_| ())
    })
    .await;
    if let Err(err) = result {
        tracing::warn!(repo = %info.id.0, "auto-snapshot failed: {err:#}");
    }
}
