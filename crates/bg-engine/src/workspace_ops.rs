//! Workspace materialization: CoW-clone the default workspace's files (APFS
//! clonefile via `cp -Rc`), then register the clone as a real jj workspace
//! sharing the repo store. Mirrors jj-cli `workspace add`
//! (`cli/src/commands/workspace/add.rs`), except the working copy is never
//! checked out: the files are already on disk from the clone, so the clone's
//! working-copy state is `reset` (state-only) to the new working-copy commit
//! and one snapshot reconciles clone-vs-parent drift — the clone's files
//! become the new change's content, which is exactly the intended semantic.

use std::path::Path;

use jj_lib::commit::Commit;
use jj_lib::default_backend_factories::default_working_copy_factory;
use jj_lib::object_id::ObjectId as _;
use jj_lib::repo::Repo as _;
use jj_lib::ref_name::{WorkspaceName, WorkspaceNameBuf};
use jj_lib::workspace::Workspace;
use jj_lib::workspace_store::{SimpleWorkspaceStore, WorkspaceStore as _};

use crate::snapshot::{short_change_id, short_commit_id};

impl crate::RepoEngine {
    /// Creates a new workspace named `name` at `dest` whose working-copy
    /// commit is a child of `at_change` (default: the first parent of the
    /// default workspace's working-copy commit — "start from what default is
    /// based on"). `dest` is materialized as a CoW clone of the default
    /// workspace's files, so build artifacts (node_modules, target, ...) come
    /// along for free.
    pub async fn add_workspace(
        &mut self,
        name: &str,
        dest: &Path,
        at_change: Option<&str>,
    ) -> anyhow::Result<bg_proto::WorkspaceInfo> {
        self.validate_new_workspace_name(name)?;
        self.validate_dest(dest)?;

        let parent = match at_change {
            Some(prefix) => self.resolve_change(prefix)?,
            None => {
                let wc = self.wc_commit("default")?;
                let parent_id = wc.parent_ids().first().ok_or_else(|| {
                    anyhow::anyhow!("working-copy commit {} has no parents", wc.id().hex())
                })?;
                self.repo.store().get_commit(parent_id)?
            }
        };

        clonefile_dir(&self.root, dest)?;

        match self.register_clone(name, dest, &parent).await {
            Ok(info) => Ok(info),
            Err(err) => Err(self.rollback_failed_workspace(name, dest, err).await),
        }
    }

    /// Every registered workspace, sorted by name. Id fields are left empty
    /// for a workspace whose working-copy commit cannot be resolved (should
    /// not happen; listing must not fail over one broken workspace).
    pub fn list_workspaces(&self) -> Vec<bg_proto::WorkspaceInfo> {
        let mut names: Vec<&String> = self.workspaces.keys().collect();
        names.sort();
        names
            .into_iter()
            .map(|name| {
                let path = self.workspaces[name].workspace_root().to_path_buf();
                match self.wc_commit(name) {
                    Ok(wc) => bg_proto::WorkspaceInfo {
                        name: name.clone(),
                        path,
                        change_id: short_change_id(&wc),
                        commit_id: short_commit_id(&wc),
                        description: wc.description().to_string(),
                    },
                    Err(_) => bg_proto::WorkspaceInfo {
                        name: name.clone(),
                        path,
                        change_id: String::new(),
                        commit_id: String::new(),
                        description: String::new(),
                    },
                }
            })
            .collect()
    }

    /// Registers the already-cloned `dest` as a jj workspace, mirroring
    /// jj-cli `workspace add`:
    /// 1. `Workspace::init_workspace_with_existing_repo` — creates `.jj` in
    ///    the clone (pointing at the primary repo), adds the name to the repo
    ///    workspace store, and commits an op with a placeholder working-copy
    ///    commit on the root commit.
    /// 2. A transaction `check_out`s `parent` for the workspace: the real
    ///    working-copy commit. `check_out`'s `edit()` abandons the
    ///    placeholder; `rebase_descendants` settles that before `commit()`
    ///    (which asserts `!has_rewrites()`). Nothing else is rebased — no
    ///    other workspace's wc commit is rewritten by this tx.
    /// 3. `sync_wc_after_tx`: state-only `reset` of the clone's working copy
    ///    to the new wc commit — no checkout writes, files are already there.
    /// 4. One `snapshot`: the clone's files (default's current state) become
    ///    the new change's content on top of `parent`.
    async fn register_clone(
        &mut self,
        name: &str,
        dest: &Path,
        parent: &Commit,
    ) -> anyhow::Result<bg_proto::WorkspaceInfo> {
        let ws_name: WorkspaceNameBuf = name.into();
        let repo_path = self.workspaces["default"].repo_path().to_path_buf();
        let factory = default_working_copy_factory();
        let (new_ws, repo) = Workspace::init_workspace_with_existing_repo(
            dest,
            &repo_path,
            &self.repo,
            factory.as_ref(),
            ws_name.clone(),
        )
        .await?;
        // Advance self.repo per committed op so op heads stay linear even if
        // a later step fails (the rollback tx then starts from the right op).
        self.repo = repo;
        self.workspaces.insert(name.to_string(), new_ws);

        let mut tx = self.repo.start_transaction();
        tx.repo_mut().check_out(ws_name, parent).await?;
        tx.repo_mut().rebase_descendants().await?;
        self.repo = tx.commit(format!("create workspace {name}")).await?;

        self.sync_wc_after_tx(name).await?;
        self.snapshot(name).await?;

        let wc = self.wc_commit(name)?;
        Ok(bg_proto::WorkspaceInfo {
            name: name.to_string(),
            path: self.workspaces[name].workspace_root().to_path_buf(),
            change_id: short_change_id(&wc),
            commit_id: short_commit_id(&wc),
            description: wc.description().to_string(),
        })
    }

    /// Best-effort compensation for a partial `add_workspace` failure: drop
    /// the in-memory entry, delete the clone, and — if the registration op
    /// already committed — forget the workspace again (mirrors jj-cli
    /// `workspace forget`). Rollback failures are chained onto the original
    /// error rather than replacing it.
    async fn rollback_failed_workspace(
        &mut self,
        name: &str,
        dest: &Path,
        err: anyhow::Error,
    ) -> anyhow::Error {
        self.workspaces.remove(name);
        if dest.exists()
            && let Err(rm_err) = std::fs::remove_dir_all(dest)
        {
            return err.context(format!(
                "rollback also failed: could not remove clone {}: {rm_err}",
                dest.display()
            ));
        }
        let ws_name: WorkspaceNameBuf = name.into();
        if self.repo.view().get_wc_commit_id(&ws_name).is_some()
            && let Err(forget_err) = self.forget_workspace_tx(&ws_name).await
        {
            return err.context(format!(
                "rollback also failed: could not forget half-registered workspace {name:?} \
                 (recover with `jj workspace forget {name}`): {forget_err:#}"
            ));
        }
        err
    }

    /// Removes a workspace's working-copy commit from the view and its entry
    /// from the repo workspace store (jj-cli `workspace forget`,
    /// `cli/src/commands/workspace/forget.rs`).
    async fn forget_workspace_tx(&mut self, ws_name: &WorkspaceName) -> anyhow::Result<()> {
        let mut tx = self.repo.start_transaction();
        tx.repo_mut().remove_wc_commit(ws_name).await?;
        // remove_wc_commit may abandon the wc commit, which registers a
        // rewrite; commit() asserts !has_rewrites().
        tx.repo_mut().rebase_descendants().await?;
        self.repo = tx.commit(format!("forget workspace {}", ws_name.as_str())).await?;
        let store = SimpleWorkspaceStore::load(self.workspaces["default"].repo_path())?;
        store.forget(&[ws_name])?; // retain-based: no-op if never added
        Ok(())
    }

    /// Rejects empty names, path-separator names, `.`/`..`, and duplicates
    /// (both the in-memory map and the repo view, which also covers
    /// "default").
    fn validate_new_workspace_name(&self, name: &str) -> anyhow::Result<()> {
        if name.is_empty() {
            return Err(crate::EngineError::Invalid("workspace name cannot be empty".into()).into());
        }
        if name == "." || name == ".." || name.contains(std::path::is_separator) {
            return Err(crate::EngineError::Invalid(format!("invalid workspace name: {name:?}")).into());
        }
        let ws_name: WorkspaceNameBuf = name.into();
        if self.workspaces.contains_key(name)
            || self.repo.view().get_wc_commit_id(&ws_name).is_some()
        {
            return Err(crate::EngineError::Invalid(format!("workspace {name:?} already exists")).into());
        }
        Ok(())
    }

    /// The destination must not exist and must not nest with the repo root
    /// (cloning the root into itself, or shadowing it). Lexical check only —
    /// callers pass absolute paths.
    fn validate_dest(&self, dest: &Path) -> anyhow::Result<()> {
        if dest.exists() {
            return Err(crate::EngineError::Invalid(format!(
                "destination exists: {}",
                dest.display()
            ))
            .into());
        }
        if dest.starts_with(&self.root) || self.root.starts_with(dest) {
            return Err(crate::EngineError::Invalid(format!(
                "destination {} must not nest with the repo root {}",
                dest.display(),
                self.root.display()
            ))
            .into());
        }
        Ok(())
    }
}

/// Copies `src` to `dest` via clonefile(2) (`cp -Rc`, CoW on APFS — off-APFS
/// or cross-volume this errors, which is fine for v1), then strips the
/// clone's `.jj` so it never impersonates the source workspace.
fn clonefile_dir(src: &Path, dest: &Path) -> anyhow::Result<()> {
    let status = std::process::Command::new("/bin/cp")
        .arg("-Rc")
        .arg(src)
        .arg(dest)
        .status()?;
    anyhow::ensure!(
        status.success(),
        "cp -Rc {} {} failed (clonefile needs src and dest on the same APFS volume)",
        src.display(),
        dest.display()
    );
    let jj_dir = dest.join(".jj");
    if jj_dir.exists() {
        std::fs::remove_dir_all(&jj_dir)?;
    }
    Ok(())
}
