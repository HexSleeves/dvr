use jj_lib::commit::Commit;
use jj_lib::gitignore::GitIgnoreFile;
use jj_lib::matchers::{EverythingMatcher, NothingMatcher};
use jj_lib::object_id::ObjectId;
use jj_lib::working_copy::SnapshotOptions;

/// 12-char rendering of a commit's change id in jj's "reverse hex" alphabet
/// (z-k), matching what the `jj` CLI displays. (`ObjectId::hex()` would be
/// plain forward hex; `ChangeId::reverse_hex()` is the CLI-visible form.)
pub(crate) fn short_change_id(commit: &Commit) -> String {
    commit.change_id().reverse_hex().chars().take(12).collect()
}

/// 12-char hex prefix of a commit's commit id.
pub(crate) fn short_commit_id(commit: &Commit) -> String {
    commit.id().hex().chars().take(12).collect()
}

impl crate::RepoEngine {
    /// Snapshots one workspace's working-copy tree into a new commit,
    /// mirroring jj-cli's `maybe_snapshot` / `snapshot_working_copy`
    /// (`cli/src/cli_util.rs`). Returns whether anything changed.
    pub async fn snapshot(&mut self, ws: &str) -> anyhow::Result<bool> {
        let wc_commit = self.wc_commit(ws)?;
        let workspace = self
            .workspaces
            .get_mut(ws)
            .ok_or_else(|| crate::EngineError::NotFound(format!("no workspace {ws}")))?;
        let workspace_name = workspace.workspace_name().to_owned();

        let mut locked_ws = workspace.start_working_copy_mutation().await?;
        let options = SnapshotOptions {
            base_ignores: GitIgnoreFile::empty(),
            progress: None,
            start_tracking_matcher: &EverythingMatcher,
            force_tracking_matcher: &NothingMatcher,
            max_new_file_size: 64 * 1024 * 1024,
        };
        let (new_tree, _stats) = locked_ws.locked_wc().snapshot(&options).await?;

        // `MergedTree` has no `id()`/`tree_id()` accessor in 0.44; compare via
        // `tree_ids_and_labels()` instead (matches jj-cli's own comparison).
        if new_tree.tree_ids_and_labels() == wc_commit.tree().tree_ids_and_labels() {
            locked_ws.finish(self.repo.op_id().clone()).await?;
            return Ok(false);
        }

        let mut tx = self.repo.start_transaction();
        tx.set_is_snapshot(true);
        let mut_repo = tx.repo_mut();
        let new_wc_commit = mut_repo
            .rewrite_commit(&wc_commit)
            .set_tree(new_tree)
            .write()
            .await?;
        // rewrite_commit() alone doesn't repoint the workspace at the new
        // commit -- jj-cli's snapshot_working_copy() does this explicitly via
        // MutableRepo::set_wc_commit() before rebasing descendants.
        mut_repo.set_wc_commit(workspace_name, new_wc_commit.id().clone())?;
        // write() leaves the commit registered as a rewrite; commit() asserts
        // !has_rewrites(), so descendants must be rebased first.
        mut_repo.rebase_descendants().await?;

        let new_repo = tx.commit(format!("snapshot workspace {ws}")).await?;
        locked_ws.finish(new_repo.op_id().clone()).await?;
        self.repo = new_repo;
        Ok(true)
    }

    /// Snapshots every registered workspace. Returns whether any of them had
    /// changes.
    pub async fn snapshot_all(&mut self) -> anyhow::Result<bool> {
        let names: Vec<String> = self.workspaces.keys().cloned().collect();
        let mut any = false;
        for n in names {
            any |= self.snapshot(&n).await?;
        }
        Ok(any)
    }
}
