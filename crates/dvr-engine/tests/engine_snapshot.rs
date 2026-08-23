mod common;
use dvr_engine::{RepoEngine, settings::make_settings};

async fn engine_in(dir: &std::path::Path) -> RepoEngine {
    common::fixture_git_repo(dir);
    RepoEngine::open_or_init(dir, make_settings("T", "t@t").unwrap()).await.unwrap()
}

#[tokio::test]
async fn snapshot_captures_new_file() {
    let dir = tempfile::tempdir().unwrap();
    let mut engine = engine_in(dir.path()).await;
    assert!(!engine.snapshot("default").await.unwrap(), "clean tree should be a no-op");
    std::fs::write(dir.path().join("hello.txt"), "hi\n").unwrap();
    assert!(engine.snapshot("default").await.unwrap(), "new file must register as a change");
    let status = engine.status("demo").unwrap();
    let ws = &status.workspaces[0];
    assert_eq!(ws.changed_files.len(), 1);
    assert_eq!(ws.changed_files[0].path, "hello.txt");
    assert_eq!(ws.changed_files[0].kind, dvr_proto::ChangeKind::Added);
}

#[tokio::test]
async fn snapshot_twice_is_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    let mut engine = engine_in(dir.path()).await;
    std::fs::write(dir.path().join("a.txt"), "1").unwrap();
    assert!(engine.snapshot("default").await.unwrap());
    assert!(!engine.snapshot("default").await.unwrap());
}
