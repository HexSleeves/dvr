mod common;
use bg_engine::{RepoEngine, settings::make_settings};

#[tokio::test]
async fn ws_new_is_cow_fast_and_independent() {
    let dir = tempfile::tempdir_in("/tmp").unwrap(); // same volume as /tmp clones; APFS
    let root = dir.path().join("main");
    std::fs::create_dir(&root).unwrap();
    common::fixture_git_repo(&root);
    // simulate a heavy dir that must NOT be re-created from scratch:
    std::fs::write(root.join(".git/info/exclude"), "node_modules/\n").unwrap();
    std::fs::create_dir_all(root.join("node_modules/dep")).unwrap();
    std::fs::write(root.join("node_modules/dep/index.js"), "x".repeat(1024)).unwrap();

    let mut engine = RepoEngine::open_or_init(&root, make_settings("T", "t@t").unwrap()).await.unwrap();
    engine.snapshot("default").await.unwrap();

    let dest = dir.path().join("agent1");
    let info = engine.add_workspace("agent1", &dest, None).await.unwrap();
    assert_eq!(info.name, "agent1");
    assert!(dest.join("README.md").is_file());
    assert!(dest.join("node_modules/dep/index.js").is_file(), "CoW clone carries build artifacts");
    assert!(dest.join(".jj").exists(), "clone is a real jj workspace");
    let git_status = std::process::Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(&dest)
        .output()
        .unwrap();
    assert!(git_status.status.success());
    assert!(
        git_status.stdout.is_empty(),
        "registering the clone must not leave .jj untracked: {}",
        String::from_utf8_lossy(&git_status.stdout)
    );

    // Independence: edit in agent1, snapshot; default's wc unchanged.
    std::fs::write(dest.join("agent-work.txt"), "hi").unwrap();
    engine.snapshot("agent1").await.unwrap();
    let wc_default = engine.wc_commit("default").unwrap();
    let wc_agent = engine.wc_commit("agent1").unwrap();
    assert_ne!(wc_default.id(), wc_agent.id());
    assert!(engine.read_file("@", "agent-work.txt").await.is_err(), "default @ must not contain agent1's file");
}

#[tokio::test]
async fn two_workspaces_on_same_change_is_allowed() {
    let dir = tempfile::tempdir_in("/tmp").unwrap();
    let root = dir.path().join("main");
    std::fs::create_dir(&root).unwrap();
    common::fixture_git_repo(&root);
    let mut engine = RepoEngine::open_or_init(&root, make_settings("T", "t@t").unwrap()).await.unwrap();
    engine.snapshot("default").await.unwrap();
    let base = engine.log(10).unwrap().into_iter().find(|e| !e.is_working_copy).unwrap().change_id;
    engine.add_workspace("a", &dir.path().join("a"), Some(&base)).await.unwrap();
    engine.add_workspace("b", &dir.path().join("b"), Some(&base)).await.unwrap();
    assert_eq!(engine.list_workspaces().len(), 3);
}

#[tokio::test]
async fn workspaces_survive_reopen() {
    let dir = tempfile::tempdir_in("/tmp").unwrap();
    let root = dir.path().join("main");
    std::fs::create_dir(&root).unwrap();
    common::fixture_git_repo(&root);
    let mut engine = RepoEngine::open_or_init(&root, make_settings("T", "t@t").unwrap()).await.unwrap();
    engine.snapshot("default").await.unwrap();
    engine.add_workspace("agent1", &dir.path().join("agent1"), None).await.unwrap();
    drop(engine);

    // Rehydration: a fresh open must see the extra workspace (crash safety —
    // the daemon reopens engines from repos.json after a restart).
    let mut engine = RepoEngine::open_or_init(&root, make_settings("T", "t@t").unwrap()).await.unwrap();
    let names: Vec<String> = engine.list_workspaces().into_iter().map(|w| w.name).collect();
    assert_eq!(names, vec!["agent1", "default"]);
    std::fs::write(dir.path().join("agent1/reopen.txt"), "hi").unwrap();
    assert!(engine.snapshot("agent1").await.unwrap(), "snapshot must work in the rehydrated clone");
    drop(engine);

    // A workspace whose directory vanished is skipped (warned), not fatal.
    std::fs::remove_dir_all(dir.path().join("agent1")).unwrap();
    let engine = RepoEngine::open_or_init(&root, make_settings("T", "t@t").unwrap()).await.unwrap();
    let names: Vec<String> = engine.list_workspaces().into_iter().map(|w| w.name).collect();
    assert_eq!(names, vec!["default"]);
}

#[tokio::test]
async fn add_workspace_rejects_bad_names_and_existing_dest() {
    let dir = tempfile::tempdir_in("/tmp").unwrap();
    let root = dir.path().join("main");
    std::fs::create_dir(&root).unwrap();
    common::fixture_git_repo(&root);
    let mut engine = RepoEngine::open_or_init(&root, make_settings("T", "t@t").unwrap()).await.unwrap();
    engine.snapshot("default").await.unwrap();

    let is_invalid = |err: &anyhow::Error| {
        matches!(err.downcast_ref::<bg_engine::EngineError>(), Some(bg_engine::EngineError::Invalid(_)))
    };
    for bad in ["", "default", "a/b"] {
        let err = engine.add_workspace(bad, &dir.path().join("x"), None).await.unwrap_err();
        assert!(is_invalid(&err), "name {bad:?} must be rejected as invalid: {err:#}");
        assert!(!dir.path().join("x").exists(), "no clone may be left behind for {bad:?}");
    }

    // Existing destination is refused before anything is cloned or registered.
    std::fs::create_dir(dir.path().join("exists")).unwrap();
    let err = engine.add_workspace("ok", &dir.path().join("exists"), None).await.unwrap_err();
    assert!(is_invalid(&err), "existing dest must be rejected: {err:#}");

    assert_eq!(engine.list_workspaces().len(), 1, "failed attempts must not register workspaces");
}
