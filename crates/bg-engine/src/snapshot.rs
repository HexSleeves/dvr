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

fn ordered_workspace_names(mut names: Vec<String>) -> Vec<String> {
    names.sort();
    if let Some(default) = names.iter().position(|name| name == "default") {
        names.swap(0, default);
    }
    names
}

impl crate::RepoEngine {
    /// Snapshots one workspace, mirroring jj-cli's colocated `snapshot_impl`
    /// (`cli/src/cli_util.rs`): for the default workspace (the one sharing
    /// its working copy with `.git`), the Git HEAD is imported BEFORE the
    /// tree snapshot — so an external `git commit`/`switch` moves the
    /// working-copy parent instead of being absorbed as a file delta — and
    /// Git refs are imported after. Returns whether anything changed.
    pub async fn snapshot(&mut self, ws: &str) -> anyhow::Result<bool> {
        let colocated = ws == "default";
        let mut changed = false;
        if colocated {
            changed |= self.import_head_from_git().await?;
        }
        changed |= self.snapshot_tree(ws).await?;
        if colocated {
            changed |= self.import_refs_from_git().await?;
        }
        Ok(changed)
    }

    /// Snapshots one workspace's working-copy tree into a new commit,
    /// mirroring jj-cli's `snapshot_working_copy` (`cli/src/cli_util.rs`).
    /// Returns whether anything changed.
    async fn snapshot_tree(&mut self, ws: &str) -> anyhow::Result<bool> {
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
    /// changes. A workspace whose directory vanished (`rm -rf` is the
    /// documented way to drop one) is skipped with a warning — same policy as
    /// `load_extra_workspaces` — so one deleted clone can't take down
    /// status/log/snapshot for the whole repo. It stays listed: its changes
    /// live in the store and the view still knows it.
    pub async fn snapshot_all(&mut self) -> anyhow::Result<bool> {
        // The default snapshot may import Git or rewrite its working-copy
        // commit and rebase descendant workspace commits. Reconcile every
        // secondary only after those repo-wide effects have settled.
        let names = ordered_workspace_names(self.workspaces.keys().cloned().collect());
        let mut any = false;
        for n in names {
            let root = self.workspaces[&n].workspace_root();
            if !root.is_dir() {
                tracing::warn!(workspace = %n, root = %root.display(),
                    "skipping snapshot: workspace directory vanished");
                continue;
            }
            any |= self.snapshot(&n).await?;
        }
        Ok(any)
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn default_workspace_snapshots_before_secondaries() {
        let names = vec!["zeta".to_string(), "default".to_string(), "alpha".to_string()];
        assert_eq!(
            super::ordered_workspace_names(names),
            ["default", "alpha", "zeta"]
        );
    }
}
