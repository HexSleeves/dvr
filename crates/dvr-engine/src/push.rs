//! Push with guardrails.
//!
//! This module is the ONLY place in the codebase allowed to spawn `git push`
//! (spec §5 "never auto-push") — enforced by the grep test in
//! `tests/engine_push.rs`. Every push names an explicit remote and bookmark
//! (spec §5 "no implicit branch tracking"): nothing is inferred, and creating
//! a branch that doesn't exist on the remote requires an explicit
//! `create=true`.

use std::process::Command;

use jj_lib::op_store::RefTarget;
use jj_lib::ref_name::RefName;

use crate::EngineError;
use crate::snapshot::short_commit_id;

impl crate::RepoEngine {
    /// Pushes exactly one bookmark to exactly one remote.
    ///
    /// Flow: resolve the target commit (default: the working-copy commit of
    /// `ws`) → guardrails (description present, no conflicts, remote branch
    /// existence vs `create`) → set the local bookmark and `export_refs` in
    /// one transaction so the colocated `.git` sees the branch → spawn
    /// `git -C <root> push <remote> <bookmark>`.
    pub async fn push(
        &mut self,
        ws: &str,
        req: &dvr_proto::PushRequest,
    ) -> anyhow::Result<dvr_proto::PushResponse> {
        // Explicit destination only. Names that are empty, contain
        // whitespace, look like an option (`-...`), or smuggle a refspec
        // (`a:b`) are rejected up front; everything else is left to git's own
        // refname validation, which surfaces through the subprocess error.
        validate_name("remote", &req.remote)?;
        validate_name("bookmark", &req.bookmark)?;

        // The watcher is debounced, so an edit followed immediately by a
        // default-target push may not be in the stored working-copy commit
        // yet. Snapshot while the daemon still holds this repo's engine lock
        // before selecting the commit to publish. An explicit change id stays
        // exact and does not absorb unrelated workspace edits.
        if req.change_id.is_none() {
            self.snapshot(ws).await?;
        }

        // Validates `ws` up front even for change_id-targeted pushes, so a bad
        // workspace name can never fail AFTER the bookmark transaction.
        let wc = self.wc_commit(ws)?;
        let target = match req.change_id.as_deref() {
            Some(prefix) => self.resolve_change(prefix)?,
            None => wc,
        };
        let short = short_commit_id(&target);

        // Guardrail: never push an undescribed change.
        if target.description().trim().is_empty() {
            return Err(EngineError::Guardrail(format!(
                "refusing to push {short}: it has no description (run `dvr describe` first)"
            ))
            .into());
        }
        // Guardrail: never push unresolved conflicts (cheap: tree-level check).
        if target.has_conflict() {
            return Err(EngineError::Guardrail(format!(
                "refusing to push {short}: it has unresolved conflicts"
            ))
            .into());
        }

        // Guardrail: first push to a branch that doesn't exist on the remote
        // requires an explicit create=true. Checked before any local ref is
        // written so a refusal leaves no trace. The pattern is the FULL ref
        // path and the reply is matched exactly — a bare bookmark pattern
        // tail-matches path components (`x` would match `refs/heads/feat/x`)
        // and could falsely report the branch as existing, silently skipping
        // this guardrail. Known limitation: the check is a snapshot; a branch
        // created/deleted on the remote between it and the push below is not
        // re-detected.
        let branch_ref = format!("refs/heads/{}", req.bookmark);
        let ls = git(&self.root, &["ls-remote", "--heads", &req.remote, &branch_ref])?;
        let exists = ls
            .stdout
            .lines()
            // `<oid>\t<refname>` per line.
            .filter_map(|l| l.split('\t').nth(1))
            .any(|r| r == branch_ref);
        if !exists && !req.create {
            return Err(EngineError::Guardrail(format!(
                "refusing to push: branch {bookmark:?} does not exist on remote {remote:?} \
                 (pass create=true to create {remote}/{bookmark})",
                remote = req.remote,
                bookmark = req.bookmark,
            ))
            .into());
        }

        // Point the local bookmark at the target and export it to the
        // colocated .git inside the same transaction, so `git push` below
        // sees refs/heads/<bookmark> at exactly this commit.
        let name = RefName::new(&req.bookmark);
        let mut tx = self.repo.start_transaction();
        tx.repo_mut().set_local_bookmark_target(name, RefTarget::normal(target.id().clone()));
        let stats = jj_lib::git::export_refs(tx.repo_mut())?;
        // Only OUR bookmark failing to export is fatal; unrelated stragglers
        // (e.g. refs conflicted since an earlier import) don't block a push
        // that never touches them.
        if let Some((symbol, reason)) =
            stats.failed_bookmarks.iter().find(|(s, _)| s.name.as_str() == req.bookmark)
        {
            anyhow::bail!("failed to export bookmark {symbol} to git: {reason}");
        }
        self.repo =
            tx.commit(format!("push {short} to {}/{}", req.remote, req.bookmark)).await?;
        self.sync_wc_after_tx(ws).await?;

        // The one and only `git push` in the codebase: explicit remote and a
        // fully-qualified src:dst refspec, so git cannot reinterpret the name
        // (e.g. `HEAD` resolving to whatever branch is checked out).
        let refspec = format!("{branch_ref}:{branch_ref}");
        git(&self.root, &["push", &req.remote, &refspec])?;

        Ok(dvr_proto::PushResponse {
            remote: req.remote.clone(),
            bookmark: req.bookmark.clone(),
            commit_id: short,
        })
    }
}

/// Rejects names that could change what the git subprocess does: empty or
/// whitespace-bearing names, option-shaped names (`-...`), refspec separators
/// (`:`), and glob characters (`*?[` — a `*` bookmark would build the
/// wildcard refspec `refs/heads/*:refs/heads/*` and push everything; the
/// never-push-everything guarantee must not rest on jj-lib's export-time
/// refname validation).
fn validate_name(what: &str, value: &str) -> anyhow::Result<()> {
    let bad = value.is_empty()
        || value.chars().any(char::is_whitespace)
        || value.starts_with('-')
        || value.chars().any(|c| matches!(c, ':' | '*' | '?' | '['));
    if bad {
        return Err(EngineError::Invalid(format!("invalid {what} name: {value:?}")).into());
    }
    Ok(())
}

struct GitOutput {
    stdout: String,
}

/// Runs `git -C <root> <args>`, failing with the subprocess stderr verbatim
/// so it surfaces unchanged in `ApiError.message`.
fn git(root: &std::path::Path, args: &[&str]) -> anyhow::Result<GitOutput> {
    let out = Command::new("git").arg("-C").arg(root).args(args).output()?;
    if !out.status.success() {
        anyhow::bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim_end()
        );
    }
    Ok(GitOutput { stdout: String::from_utf8_lossy(&out.stdout).into_owned() })
}
