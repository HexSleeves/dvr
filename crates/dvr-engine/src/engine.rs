use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use jj_lib::backend::CommitId;
use jj_lib::commit::Commit;

use jj_lib::default_backend_factories::{default_backend_factories, default_working_copy_factories};
use jj_lib::git::{GitImportOptions, import_refs};
use jj_lib::matchers::EverythingMatcher;
use jj_lib::merge::Merge;
use jj_lib::merged_tree::{MergedTree, TreeDiffIterator};
use jj_lib::object_id::ObjectId;
use jj_lib::repo::{ReadonlyRepo, Repo};
use jj_lib::settings::UserSettings;
use jj_lib::workspace::Workspace;

use crate::snapshot::{short_change_id, short_commit_id};

pub struct RepoEngine {
    pub(crate) root: PathBuf,
    pub(crate) repo: Arc<ReadonlyRepo>,
    pub(crate) workspaces: HashMap<String, Workspace>,
}

impl RepoEngine {
    pub async fn open_or_init(root: &Path, settings: UserSettings) -> anyhow::Result<Self> {
        anyhow::ensure!(root.join(".git").exists(), "not a git repo: {}", root.display());

        let (workspace, repo) = if root.join(".jj").exists() {
            let workspace = Workspace::load(
                &settings,
                root,
                &default_backend_factories(),
                &default_working_copy_factories(),
            )?;
            let repo = workspace.repo_loader().load_at_head().await?;
            (workspace, repo)
        } else {
            let (mut workspace, repo) =
                Workspace::init_external_git(&settings, root, &root.join(".git")).await?;
            let repo = import_git_refs(repo).await?;
            let repo = import_git_head(&mut workspace, repo).await?;
            (workspace, repo)
        };

        // Hide `.jj` from git so registering a repo never dirties its `git
        // status` — same as jj-cli's `maybe_add_gitignore` for colocated
        // repos (cli/src/commands/git/mod.rs writes "/*\n"). If-absent, not
        // just on fresh init: it also heals repos initialized before this
        // was written, without clobbering a user-edited file.
        let jj_gitignore = root.join(".jj").join(".gitignore");
        if !jj_gitignore.exists() {
            std::fs::write(&jj_gitignore, "/*\n")?;
        }

        let mut workspaces = HashMap::new();
        // Rehydrate the other workspaces of this repo (created by
        // add_workspace in an earlier daemon life) from the view + the repo
        // workspace store, so restarts keep watching/snapshotting them.
        crate::workspace_ops::load_extra_workspaces(&settings, &workspace, &repo, &mut workspaces)?;
        workspaces.insert("default".to_string(), workspace);

        Ok(Self { root: root.to_path_buf(), repo, workspaces })
    }

    /// Reports the current state of every workspace: working-copy change /
    /// commit ids, description, first-parent change id, and the files changed
    /// relative to that first parent.
    pub fn status(&self, repo_id: &str) -> anyhow::Result<dvr_proto::StatusResponse> {
        let mut names: Vec<&String> = self.workspaces.keys().collect();
        names.sort();

        let mut workspaces = Vec::with_capacity(names.len());
        for name in names {
            let workspace = &self.workspaces[name];
            let wc = self.wc_commit(name)?;
            let parent_id = wc
                .parent_ids()
                .first()
                .ok_or_else(|| anyhow::anyhow!("working-copy commit {} has no parents", wc.id().hex()))?;
            let parent = self.repo.store().get_commit(parent_id)?;

            workspaces.push(dvr_proto::WorkspaceStatus {
                info: dvr_proto::WorkspaceInfo {
                    name: name.clone(),
                    path: workspace.workspace_root().to_path_buf(),
                    change_id: short_change_id(&wc),
                    commit_id: short_commit_id(&wc),
                    description: wc.description().to_string(),
                },
                parent_change_id: short_change_id(&parent),
                changed_files: changed_files(&parent.tree(), &wc.tree())?,
            });
        }

        Ok(dvr_proto::StatusResponse {
            repo: dvr_proto::RepoInfo {
                id: dvr_proto::RepoId(repo_id.to_string()),
                root: self.root.clone(),
            },
            workspaces,
        })
    }

    /// Returns up to `limit` visible commits, newest-first, with working-copy
    /// commits flagged and local bookmark names attached.
    pub fn log(&self, limit: usize) -> anyhow::Result<Vec<dvr_proto::LogEntry>> {
        let commits = self.visible_commits()?;
        Ok(commits.iter().take(limit).map(|c| self.log_entry_for(c)).collect())
    }

    /// Resolves a change-id (reverse-hex, as displayed) or commit-id (hex)
    /// prefix against the visible commits. Errors distinguish "not found"
    /// from "ambiguous".
    pub fn resolve_change(&self, prefix: &str) -> anyhow::Result<Commit> {
        if prefix.is_empty() {
            return Err(crate::EngineError::Invalid("empty change/commit id prefix".into()).into());
        }
        let mut matches: Vec<Commit> = self
            .visible_commits()?
            .into_iter()
            .filter(|c| {
                c.change_id().reverse_hex().starts_with(prefix) || c.id().hex().starts_with(prefix)
            })
            .collect();
        match matches.len() {
            0 => Err(crate::EngineError::NotFound(format!(
                "no visible commit matches prefix {prefix:?}"
            ))
            .into()),
            1 => Ok(matches.pop().unwrap()),
            n => Err(crate::EngineError::Invalid(format!(
                "prefix {prefix:?} is ambiguous: {n} visible commits match"
            ))
            .into()),
        }
    }

    /// Rewrites the description of the target commit (default: the workspace's
    /// working-copy commit), then re-syncs the on-disk working-copy state.
    /// Mirrors jj-cli `describe` (`cli/src/commands/describe.rs`) followed by
    /// `update_working_copy` (`cli/src/cli_util.rs`).
    pub async fn describe(
        &mut self,
        ws: &str,
        change: Option<&str>,
        message: &str,
    ) -> anyhow::Result<dvr_proto::LogEntry> {
        let target = match change {
            Some(prefix) => self.resolve_change(prefix)?,
            None => self.wc_commit(ws)?,
        };
        let mut tx = self.repo.start_transaction();
        tx.repo_mut()
            .rewrite_commit(&target)
            .set_description(message)
            .write()
            .await?;
        // write() leaves the commit registered as a rewrite; commit() asserts
        // !has_rewrites(). rebase_descendants also repoints wc commits at the
        // rewritten target (MutableRepo::update_wc_commits).
        tx.repo_mut().rebase_descendants().await?;
        self.repo = tx.commit(format!("describe {}", &target.id().hex()[..12])).await?;
        self.sync_wc_after_tx(ws).await?;
        // The change id is stable across the rewrite, so re-resolve it (full
        // reverse-hex form, immune to prefix collisions) in the new repo state
        // and reuse the log-entry builder.
        let rewritten = self.resolve_change(&target.change_id().reverse_hex())?;
        Ok(self.log_entry_for(&rewritten))
    }

    /// After any transaction that rewrote a working-copy commit: repoint the
    /// on-disk working-copy state at the (possibly rewritten) wc commit and
    /// record the new operation id. State-only (`reset` does not touch files);
    /// mirrors jj-cli `update_working_copy` (`cli/src/cli_util.rs`).
    pub(crate) async fn sync_wc_after_tx(&mut self, ws: &str) -> anyhow::Result<()> {
        let wc_commit = self.wc_commit(ws)?;
        let workspace = self
            .workspaces
            .get_mut(ws)
            .ok_or_else(|| crate::EngineError::NotFound(format!("no workspace {ws}")))?;
        let mut locked_ws = workspace.start_working_copy_mutation().await?;
        locked_ws.locked_wc().reset(&wc_commit).await?;
        locked_ws.finish(self.repo.op_id().clone()).await?;
        Ok(())
    }

    /// Maps a commit to its `dvr_proto::LogEntry`, shared by `log` and
    /// `describe`.
    pub(crate) fn log_entry_for(&self, commit: &Commit) -> dvr_proto::LogEntry {
        let view = self.repo.view();
        let bookmarks = view
            .local_bookmarks_for_commit(commit.id())
            .map(|(name, _)| name.as_str().to_string())
            .collect();
        dvr_proto::LogEntry {
            change_id: short_change_id(commit),
            commit_id: short_commit_id(commit),
            description: commit.description().to_string(),
            author_name: commit.author().name.clone(),
            author_email: commit.author().email.clone(),
            timestamp_ms: commit.committer().timestamp.timestamp.0.max(0) as u64,
            bookmarks,
            is_working_copy: view.wc_commit_ids().values().any(|id| id == commit.id()),
        }
    }

    /// All visible commits (ancestors of the view's heads and working-copy
    /// commits, root included), sorted newest-first by committer timestamp.
    /// jj-lib 0.44's `Revset` trait is stream-only, so this walks the store
    /// directly to keep `log`/`resolve_change` synchronous.
    fn visible_commits(&self) -> anyhow::Result<Vec<Commit>> {
        let view = self.repo.view();
        // Depth-first from the visible heads (+ wc commits), emitting each
        // commit on DFS exit (`Emit` carries the commit fetched on entry);
        // the reversed post-order is a topological order (every commit
        // before all of its ancestors).
        enum Step {
            Visit(CommitId),
            Emit(Commit),
        }
        let mut stack: Vec<Step> = view
            .heads()
            .iter()
            .cloned()
            .chain(view.wc_commit_ids().values().cloned())
            .map(Step::Visit)
            .collect();
        let mut seen: HashSet<CommitId> = HashSet::new();
        let mut commits = Vec::new();
        while let Some(step) = stack.pop() {
            let id = match step {
                Step::Emit(commit) => {
                    commits.push(commit);
                    continue;
                }
                Step::Visit(id) => id,
            };
            if !seen.insert(id.clone()) {
                continue;
            }
            let commit = self.repo.store().get_commit(&id)?;
            stack.push(Step::Emit(commit.clone()));
            stack.extend(commit.parent_ids().iter().cloned().map(Step::Visit));
        }
        commits.reverse();
        // Newest first — but STABLE, so commits sharing a committer second
        // (batch scripts, agents) keep the topological child-before-parent
        // order instead of falling into arbitrary id order (a dogfooding bug:
        // `dvr log` showed a parent above its child).
        commits.sort_by_key(|c| std::cmp::Reverse(c.committer().timestamp.timestamp.0));
        Ok(commits)
    }

    /// Reads the contents of `path` in the tree of the given revision.
    /// `rev` is a change/commit-id prefix (see `resolve_change`) or `"@"` for
    /// the working-copy commit of the default workspace. Errors on missing
    /// paths, conflicted paths, and non-file entries (directories, symlinks,
    /// submodules).
    pub async fn read_file(&self, rev: &str, path: &str) -> anyhow::Result<Vec<u8>> {
        let commit = if rev == "@" { self.wc_commit("default")? } else { self.resolve_change(rev)? };
        let repo_path = jj_lib::repo_path::RepoPath::from_internal_string(path)?;
        let tree = commit.tree();
        let value = tree.path_value(repo_path).await?;
        let resolved = value
            .into_resolved()
            .map_err(|_| crate::EngineError::Conflict(format!("path is conflicted: {path}")))?
            .ok_or_else(|| crate::EngineError::NotFound(format!("not found: {path}")))?;
        match resolved {
            jj_lib::backend::TreeValue::File { id, .. } => {
                let mut reader = self.repo.store().read_file(repo_path, &id).await?;
                let mut buf = Vec::new();
                futures::AsyncReadExt::read_to_end(&mut reader, &mut buf).await?;
                Ok(buf)
            }
            _ => Err(crate::EngineError::Invalid(format!("not a file: {path}")).into()),
        }
    }

    /// Re-imports the colocated Git repo's HEAD, checking it out as the
    /// default workspace's working-copy parent if it moved. Runs BEFORE every
    /// default-workspace tree snapshot (mirroring jj-cli's colocated
    /// `snapshot_impl`, `cli/src/cli_util.rs`), so external `git
    /// commit`/`switch`/`pull` in a registered repo stay visible — and a moved
    /// HEAD's file delta is never absorbed into the current working-copy
    /// change. Cheap when nothing moved: `import_head` records no view change,
    /// so no operation is committed (jj-cli relies on the same guard).
    pub(crate) async fn import_head_from_git(&mut self) -> anyhow::Result<bool> {
        let workspace = self
            .workspaces
            .get_mut("default")
            .ok_or_else(|| crate::EngineError::NotFound("no workspace default".to_string()))?;
        let new_repo = import_git_head(workspace, self.repo.clone()).await?;
        let changed = !Arc::ptr_eq(&new_repo, &self.repo);
        self.repo = new_repo;
        Ok(changed)
    }

    /// Re-imports Git branches/tags AFTER a tree snapshot (jj-cli's order in
    /// `snapshot_impl`: the just-snapshotted working-copy commit's ref may not
    /// be exported yet, which is fine — it would be conflicted anyway). A
    /// moved ref can rewrite commits, so descendants are rebased inside
    /// `import_git_refs` and the default workspace's on-disk state is
    /// re-synced here.
    pub(crate) async fn import_refs_from_git(&mut self) -> anyhow::Result<bool> {
        let new_repo = import_git_refs(self.repo.clone()).await?;
        if Arc::ptr_eq(&new_repo, &self.repo) {
            return Ok(false);
        }
        self.repo = new_repo;
        self.sync_wc_after_tx("default").await?;
        Ok(true)
    }

    pub fn wc_commit(&self, ws: &str) -> anyhow::Result<jj_lib::commit::Commit> {
        let workspace = self
            .workspaces
            .get(ws)
            .ok_or_else(|| crate::EngineError::NotFound(format!("no workspace {ws}")))?;
        let id = self
            .repo
            .view()
            .get_wc_commit_id(workspace.workspace_name())
            .ok_or_else(|| crate::EngineError::NotFound(format!("no working-copy commit for {ws}")))?;
        Ok(self.repo.store().get_commit(id)?)
    }
}

/// Imports the Git HEAD and checks it out as the working-copy parent,
/// mirroring jj-cli's `import_git_head` (`cli/src/cli_util.rs`): the new
/// working-copy commit gets HEAD's tree and the on-disk working-copy state is
/// `reset` to it without touching files, so a subsequent snapshot of an
/// unmodified tree is a no-op.
async fn import_git_head(
    workspace: &mut Workspace,
    repo: Arc<ReadonlyRepo>,
) -> anyhow::Result<Arc<ReadonlyRepo>> {
    let mut tx = repo.start_transaction();
    jj_lib::git::import_head(tx.repo_mut()).await?;
    if !tx.repo().has_changes() {
        return Ok(repo);
    }
    let Some(head_id) = tx.repo().view().git_head().as_normal().cloned() else {
        // Unlikely: HEAD ref vanished. Mirror jj-cli: settle rewrites, commit.
        tx.repo_mut().rebase_descendants().await?;
        return Ok(tx.commit("import git head").await?);
    };
    let head_commit = tx.repo().store().get_commit(&head_id)?;
    let wc_commit = tx
        .repo_mut()
        .check_out(workspace.workspace_name().to_owned(), &head_commit)
        .await?;
    let mut locked_ws = workspace.start_working_copy_mutation().await?;
    locked_ws.locked_wc().reset(&wc_commit).await?;
    tx.repo_mut().rebase_descendants().await?;
    let new_repo = tx.commit("import git head").await?;
    locked_ws.finish(new_repo.op_id().clone()).await?;
    Ok(new_repo)
}

/// Walks the diff between two trees and maps each file entry to a
/// `FileChange`. Mirrors jj's `stream_without_trees` (`lib/src/merged_tree.rs`)
/// filtering: directory entries are dropped by treating tree values as absent,
/// and entries absent on both sides are skipped.
fn changed_files(before: &MergedTree, after: &MergedTree) -> anyhow::Result<Vec<dvr_proto::FileChange>> {
    let skip_trees = |v: jj_lib::merge::MergedTreeValue| if v.is_tree() { Merge::absent() } else { v };
    let mut out = Vec::new();
    for entry in TreeDiffIterator::new(before, after, &EverythingMatcher) {
        let diff = entry.values?.map(skip_trees);
        let kind = match (diff.before.is_present(), diff.after.is_present()) {
            (false, true) => dvr_proto::ChangeKind::Added,
            (true, false) => dvr_proto::ChangeKind::Removed,
            (true, true) => dvr_proto::ChangeKind::Modified,
            (false, false) => continue,
        };
        out.push(dvr_proto::FileChange {
            path: entry.path.as_internal_file_string().to_string(),
            kind,
        });
    }
    Ok(out)
}

/// Imports Git refs (branches/tags) into the jj view: at init so pre-existing
/// Git history is visible (jj-cli `git init --git-repo`,
/// `cli/src/commands/git/init.rs`), and per snapshot so externally moved refs
/// stay visible (jj-cli `import_git_refs`, `cli/src/cli_util.rs`). Returns
/// the SAME `Arc` when nothing changed, so callers can detect movement via
/// `Arc::ptr_eq`. Ref imports can rewrite/abandon commits, so descendants are
/// rebased before committing (`commit()` asserts `!has_rewrites()`).
async fn import_git_refs(repo: Arc<ReadonlyRepo>) -> anyhow::Result<Arc<ReadonlyRepo>> {
    let mut tx = repo.start_transaction();
    let options = GitImportOptions {
        abandon_unreachable_commits: false,
        record_synthetic_predecessors: false,
        remote_auto_track_bookmarks: Default::default(),
    };
    import_refs(tx.repo_mut(), &options).await?;
    if tx.repo().has_changes() {
        tx.repo_mut().rebase_descendants().await?;
        Ok(tx.commit("import git refs").await?)
    } else {
        Ok(repo)
    }
}
