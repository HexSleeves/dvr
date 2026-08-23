use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use jj_lib::default_backend_factories::{default_backend_factories, default_working_copy_factories};
use jj_lib::git::{GitImportOptions, import_refs};
use jj_lib::repo::{ReadonlyRepo, Repo};
use jj_lib::settings::UserSettings;
use jj_lib::workspace::Workspace;

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
            let (workspace, repo) =
                Workspace::init_external_git(&settings, root, &root.join(".git")).await?;
            let repo = import_existing_git_refs(repo).await?;
            (workspace, repo)
        };

        let mut workspaces = HashMap::new();
        workspaces.insert("default".to_string(), workspace);

        Ok(Self { root: root.to_path_buf(), settings, repo, workspaces })
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
