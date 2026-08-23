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
