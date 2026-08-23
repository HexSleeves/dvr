mod common;
use bg_engine::{settings::make_settings, RepoEngine};

#[tokio::test]
async fn reads_file_at_wc_and_errors_on_missing() {
    let dir = tempfile::tempdir().unwrap();
    common::fixture_git_repo(dir.path());
    let mut engine = RepoEngine::open_or_init(dir.path(), make_settings("T", "t@t").unwrap()).await.unwrap();
    std::fs::write(dir.path().join("data.txt"), "payload").unwrap();
    std::fs::create_dir(dir.path().join("nested")).unwrap();
    std::fs::write(dir.path().join("nested/data.txt"), "nested").unwrap();
    engine.snapshot("default").await.unwrap();
    assert_eq!(engine.read_file("@", "data.txt").await.unwrap(), b"payload");
    assert_eq!(engine.read_file("@", "README.md").await.unwrap(), b"# fixture\n");
    assert!(engine.read_file("@", "nope.txt").await.is_err());
    let err = engine.read_file("@", "nested").await.unwrap_err();
    assert!(err.to_string().contains("not a file"), "{err:#}");
    assert!(
        matches!(err.downcast_ref(), Some(bg_engine::EngineError::Invalid(_))),
        "directory reads must be invalid requests: {err:#}"
    );
}
