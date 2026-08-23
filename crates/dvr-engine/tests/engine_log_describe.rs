mod common;
use dvr_engine::{settings::make_settings, RepoEngine};

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

#[tokio::test]
async fn resolve_change_marks_empty_prefix_as_invalid() {
    let dir = tempfile::tempdir().unwrap();
    common::fixture_git_repo(dir.path());
    let engine = RepoEngine::open_or_init(dir.path(), make_settings("T", "t@t").unwrap()).await.unwrap();
    let err = engine.resolve_change("").unwrap_err();
    assert!(
        matches!(err.downcast_ref(), Some(dvr_engine::EngineError::Invalid(_))),
        "empty prefix must be an invalid request: {err:#}"
    );
}

#[tokio::test]
async fn resolve_change_marks_ambiguous_prefix_as_invalid() {
    let dir = tempfile::tempdir().unwrap();
    common::fixture_git_repo(dir.path());
    let git = |args: &[&str]| {
        let out = std::process::Command::new("git")
            .args(["-c", "user.name=t", "-c", "user.email=t@t"])
            .args(args)
            .current_dir(dir.path())
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    };
    // Seventeen visible commits force at least two ids to share a one-digit
    // prefix. Derive the prefix from the engine's public rendering so the
    // test is deterministic without depending on a particular hash.
    for n in 0..16 {
        std::fs::write(dir.path().join(format!("{n}.txt")), n.to_string()).unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-m", &format!("commit {n}")]);
    }
    let engine = RepoEngine::open_or_init(dir.path(), make_settings("T", "t@t").unwrap()).await.unwrap();
    let mut counts = std::collections::BTreeMap::new();
    for entry in engine.log(100).unwrap() {
        for prefix in [entry.change_id.chars().next().unwrap(), entry.commit_id.chars().next().unwrap()] {
            *counts.entry(prefix).or_insert(0) += 1;
        }
    }
    let prefix = counts.into_iter().find(|(_, count)| *count >= 2).unwrap().0.to_string();
    let err = engine.resolve_change(&prefix).unwrap_err();
    assert!(err.to_string().contains("ambiguous"), "{err:#}");
    assert!(
        matches!(err.downcast_ref(), Some(dvr_engine::EngineError::Invalid(_))),
        "ambiguous prefix must be an invalid request: {err:#}"
    );
}

/// Batch commits (scripts, agents) land in the same second. Dogfooding v1 on
/// this repo showed `dvr log` rendering a parent ABOVE its child when their
/// committer timestamps tied — the sort fell back to arbitrary commit-id
/// order. Log order must stay topological (child before parent) within ties.
#[tokio::test]
async fn log_orders_same_second_commits_child_first() {
    let dir = tempfile::tempdir().unwrap();
    // A 3-commit chain whose committer timestamps are all identical.
    let git = |args: &[&str]| {
        let out = std::process::Command::new("git")
            .args(["-c", "user.name=t", "-c", "user.email=t@t"])
            .args(args)
            .env("GIT_AUTHOR_DATE", "2026-01-02T03:04:05Z")
            .env("GIT_COMMITTER_DATE", "2026-01-02T03:04:05Z")
            .current_dir(dir.path())
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?} failed: {}", String::from_utf8_lossy(&out.stderr));
    };
    git(&["init", "-b", "main"]);
    for (file, msg) in [("a.txt", "first"), ("b.txt", "second"), ("c.txt", "third")] {
        std::fs::write(dir.path().join(file), msg).unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-m", msg]);
    }

    let engine = RepoEngine::open_or_init(dir.path(), make_settings("T", "t@t").unwrap()).await.unwrap();
    let log = engine.log(10).unwrap();
    assert!(log[0].is_working_copy, "wc commit (fresh timestamp) must come first: {log:?}");
    let descs: Vec<&str> = log[1..].iter().map(|e| e.description.trim()).collect();
    // Trailing "" is the jj root commit (timestamp 0, empty description).
    assert_eq!(
        descs,
        ["third", "second", "first", ""],
        "same-timestamp commits must keep topological child-before-parent order"
    );
}
