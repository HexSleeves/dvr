mod common;
use bg_engine::{RepoEngine, settings::make_settings};

#[tokio::test]
async fn init_on_existing_git_repo_creates_wc_commit() {
    let dir = tempfile::tempdir().unwrap();
    common::fixture_git_repo(dir.path());
    let settings = make_settings("Test", "test@example.com").unwrap();
    let engine = RepoEngine::open_or_init(dir.path(), settings)
        .await
        .unwrap_or_else(|e| panic!("{e:#}"));
    let wc = engine.wc_commit("default").unwrap();
    assert!(wc.description().is_empty()); // fresh working-copy commit
    assert!(dir.path().join(".jj").is_dir());
}

#[tokio::test]
async fn open_or_init_is_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    common::fixture_git_repo(dir.path());
    let settings = make_settings("Test", "test@example.com").unwrap();
    let _ = RepoEngine::open_or_init(dir.path(), settings.clone())
        .await
        .unwrap();
    let engine2 = RepoEngine::open_or_init(dir.path(), settings).await.unwrap();
    assert!(engine2.wc_commit("default").is_ok());
}

/// Registering a repo must not dirty its `git status`: jj-cli hides the `.jj`
/// dir from git in colocated repos by writing `.jj/.gitignore` = "/*\n"
/// (cli/src/commands/git/mod.rs `maybe_add_gitignore`); we do the same. The
/// repo-local alternative (editing the USER'S .gitignore) is not ours to do.
#[tokio::test]
async fn init_hides_jj_dir_from_git_status() {
    let dir = tempfile::tempdir().unwrap();
    common::fixture_git_repo(dir.path());
    let settings = make_settings("Test", "test@example.com").unwrap();
    let _engine = RepoEngine::open_or_init(dir.path(), settings).await.unwrap();

    let gitignore = std::fs::read_to_string(dir.path().join(".jj/.gitignore"))
        .expect("init must write .jj/.gitignore");
    assert_eq!(gitignore, "/*\n");

    let out = std::process::Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(out.status.success());
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "",
        "git status must stay clean after open_or_init"
    );
}
