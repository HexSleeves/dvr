use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

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
    pub(crate) settings: UserSettings,
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
            let repo = import_existing_git_refs(repo).await?;
            let repo = import_git_head(&mut workspace, repo).await?;
            (workspace, repo)
        };

        let mut workspaces = HashMap::new();
        workspaces.insert("default".to_string(), workspace);

        Ok(Self { root: root.to_path_buf(), settings, repo, workspaces })
    }

    /// Reports the current state of every workspace: working-copy change /
    /// commit ids, description, first-parent change id, and the files changed
    /// relative to that first parent.
    pub fn status(&self, repo_id: &str) -> anyhow::Result<bg_proto::StatusResponse> {
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

            workspaces.push(bg_proto::WorkspaceStatus {
                info: bg_proto::WorkspaceInfo {
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

        Ok(bg_proto::StatusResponse {
            repo: bg_proto::RepoInfo {
                id: bg_proto::RepoId(repo_id.to_string()),
                root: self.root.clone(),
            },
            workspaces,
        })
    }

    pub fn wc_commit(&self, ws: &str) -> anyhow::Result<jj_lib::commit::Commit> {
        let workspace = self
            .workspaces
            .get(ws)
            .ok_or_else(|| anyhow::anyhow!("no workspace {ws}"))?;
        let id = self
            .repo
            .view()
            .get_wc_commit_id(workspace.workspace_name())
            .ok_or_else(|| anyhow::anyhow!("no working-copy commit for {ws}"))?;
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
fn changed_files(before: &MergedTree, after: &MergedTree) -> anyhow::Result<Vec<bg_proto::FileChange>> {
    let skip_trees = |v: jj_lib::merge::MergedTreeValue| if v.is_tree() { Merge::absent() } else { v };
    let mut out = Vec::new();
    for entry in TreeDiffIterator::new(before, after, &EverythingMatcher) {
        let diff = entry.values?.map(skip_trees);
        let kind = match (diff.before.is_present(), diff.after.is_present()) {
            (false, true) => bg_proto::ChangeKind::Added,
            (true, false) => bg_proto::ChangeKind::Removed,
            (true, true) => bg_proto::ChangeKind::Modified,
            (false, false) => continue,
        };
        out.push(bg_proto::FileChange {
            path: entry.path.as_internal_file_string().to_string(),
            kind,
        });
    }
    Ok(out)
}

/// Imports existing Git refs (branches/tags) into a freshly-initialized jj
/// repo so that pre-existing Git history is visible, mirroring the sequence
/// in jj-cli's `git init --git-repo` (`cli/src/commands/git/init.rs`).
async fn import_existing_git_refs(repo: Arc<ReadonlyRepo>) -> anyhow::Result<Arc<ReadonlyRepo>> {
    let mut tx = repo.start_transaction();
    let options = GitImportOptions {
        abandon_unreachable_commits: false,
        record_synthetic_predecessors: false,
        remote_auto_track_bookmarks: Default::default(),
    };
    import_refs(tx.repo_mut(), &options).await?;
    if tx.repo().has_changes() {
        Ok(tx.commit("import git refs").await?)
    } else {
        Ok(repo)
    }
}
