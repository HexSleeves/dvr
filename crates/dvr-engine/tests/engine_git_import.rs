//! External-git interop: a registered repo is still a plain git repo, so
//! commits/branch moves made by `git` itself must be imported on the next
//! snapshot — mirroring jj-cli's colocated flow (`snapshot_impl` in
//! `cli/src/cli_util.rs`: import HEAD before the tree snapshot, import refs
//! after).

mod common;
use std::path::Path;
use std::process::Command;

use dvr_engine::{RepoEngine, settings::make_settings};

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(["-c", "user.name=t", "-c", "user.email=t@t"])
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?} failed: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// (a) An external `git commit` in a registered repo must show up in the
/// engine log after a snapshot — HEAD and the moved branch ref both.
#[tokio::test]
async fn external_git_commit_is_imported_on_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    common::fixture_git_repo(dir.path());
    let mut engine =
        RepoEngine::open_or_init(dir.path(), make_settings("T", "t@t").unwrap()).await.unwrap();

    // Entirely outside dvr: write, add, commit with plain git.
    std::fs::write(dir.path().join("external.txt"), "made by git\n").unwrap();
    git(dir.path(), &["add", "external.txt"]);
    git(dir.path(), &["commit", "-m", "external: made by git"]);

    engine.snapshot("default").await.unwrap();

    let log = engine.log(10).unwrap();
    let entry = log
        .iter()
        .find(|e| e.description.trim() == "external: made by git")
        .unwrap_or_else(|| panic!("external commit missing from log: {log:#?}"));
    // import_refs (after the snapshot) must have moved the `main` bookmark too.
    assert_eq!(entry.bookmarks, vec!["main"], "branch ref must be imported: {log:#?}");
    // Per jj colocated semantics the new HEAD becomes the working-copy parent.
    let wc = engine.wc_commit("default").unwrap();
    let head = git(dir.path(), &["rev-parse", "HEAD"]);
    assert_eq!(
        jj_lib::object_id::ObjectId::hex(wc.parent_ids().first().unwrap()),
        head,
        "working copy must sit on the new git HEAD"
    );
}

/// (b) An external `git checkout` of a different commit must NOT be absorbed
/// into the current working-copy change: jj colocated semantics reset the
/// working-copy parent to the new HEAD, so the wc change stays empty.
#[tokio::test]
async fn external_git_checkout_is_not_absorbed_into_wc_change() {
    let dir = tempfile::tempdir().unwrap();
    common::fixture_git_repo(dir.path());
    let commit1 = git(dir.path(), &["rev-parse", "HEAD"]);
    // A second commit with different content, made by git before registration
    // drift can matter.
    std::fs::write(dir.path().join("extra.txt"), "second commit\n").unwrap();
    git(dir.path(), &["add", "extra.txt"]);
    git(dir.path(), &["commit", "-m", "second"]);

    let mut engine =
        RepoEngine::open_or_init(dir.path(), make_settings("T", "t@t").unwrap()).await.unwrap();

    // External `git checkout` of the older commit: git rewrites the worktree
    // (removes extra.txt) and moves HEAD.
    git(dir.path(), &["checkout", &commit1]);

    engine.snapshot("default").await.unwrap();

    let wc = engine.wc_commit("default").unwrap();
    assert_eq!(
        jj_lib::object_id::ObjectId::hex(wc.parent_ids().first().unwrap()),
        commit1,
        "checked-out commit must become the working-copy parent"
    );
    let status = engine.status("t").unwrap();
    let default = status.workspaces.iter().find(|w| w.info.name == "default").unwrap();
    assert!(
        default.changed_files.is_empty(),
        "branch delta must not be absorbed into the wc change: {:#?}",
        default.changed_files
    );
}
