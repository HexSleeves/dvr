mod common;
use bg_engine::{settings::make_settings, RepoEngine};

#[tokio::test]
async fn log_shows_wc_and_history_and_describe_sets_message() {
    let dir = tempfile::tempdir().unwrap();
    common::fixture_git_repo(dir.path());
    let mut engine = RepoEngine::open_or_init(dir.path(), make_settings("T", "t@t").unwrap()).await.unwrap();
    std::fs::write(dir.path().join("f.txt"), "x").unwrap();
    engine.snapshot("default").await.unwrap();

    let log = engine.log(10).unwrap();
    assert!(log.iter().any(|e| e.is_working_copy));
    assert!(log.len() >= 2); // wc commit + the git "init" commit

    let wc_change = log.iter().find(|e| e.is_working_copy).unwrap().change_id.clone();
    let entry = engine.describe("default", None, "add f.txt").await.unwrap();
    assert_eq!(entry.description, "add f.txt");

    // description survives another snapshot (same change, amended)
    std::fs::write(dir.path().join("f.txt"), "xy").unwrap();
    engine.snapshot("default").await.unwrap();
    let log2 = engine.log(10).unwrap();
    let wc2 = log2.iter().find(|e| e.is_working_copy).unwrap();
    assert_eq!(wc2.description, "add f.txt");
    assert_eq!(wc2.change_id, wc_change, "change id is stable across amends");
}

#[tokio::test]
async fn resolve_change_rejects_unknown_prefix() {
    let dir = tempfile::tempdir().unwrap();
    common::fixture_git_repo(dir.path());
    let engine = RepoEngine::open_or_init(dir.path(), make_settings("T", "t@t").unwrap()).await.unwrap();
    assert!(engine.resolve_change("zzzzzzzzzzzz9999").is_err());
}
