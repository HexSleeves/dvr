//! Crash-safety: edits made while the daemon is down must land in the oplog
//! during startup — `run_with_dir` re-scans every registered repo BEFORE
//! binding the socket (spec: error-handling behavior), so the first request a
//! restarted daemon answers already sees the offline drift.

mod common;
use serde_json::json;

#[tokio::test]
async fn changes_made_while_daemon_down_are_snapshotted_on_startup() {
    let state = common::fresh_state_dir();
    let repo_dir = common::fixture_repo("/tmp", "bgtest-restart");

    // First daemon: register the repo, then crash.
    {
        let d = common::spawn_daemon_in(state.path()).await;
        let (st, info) =
            common::req_json(&d.socket, "POST", "/repos", Some(json!({"path": repo_dir}))).await;
        assert_eq!(st, 200, "{info}");
        assert_eq!(info["id"], "bgtest-restart");
        d.shutdown().await; // abort the daemon task, remove the socket
    }

    // Daemon is DOWN; edit files.
    std::fs::write(std::path::Path::new(&repo_dir).join("offline.txt"), "made while down")
        .unwrap();

    // Restart on the same state dir. /health only answers after the startup
    // re-scan (run_with_dir snapshots before binding the socket), and /file is
    // side-effect free — it never snapshots — so a 200 here proves the offline
    // edit was captured by the re-scan, not by this read.
    let d = common::spawn_daemon_in(state.path()).await;
    let (st, body) =
        common::req_raw(&d.socket, "/repos/bgtest-restart/file?rev=@&path=offline.txt").await;
    assert_eq!(
        st,
        200,
        "startup re-scan must have captured offline edits: {}",
        String::from_utf8_lossy(&body)
    );
    assert_eq!(body, b"made while down");
}

#[tokio::test]
async fn startup_scan_surfaces_repo_snapshot_failures() {
    let state_dir = common::fresh_state_dir();
    let repo_dir = common::fixture_repo("/tmp", "bgtest-startup-scan-failure");
    let state = bg_daemon::state::DaemonState::load(state_dir.path()).await.unwrap();
    state.register(std::path::Path::new(&repo_dir)).await.unwrap();

    // Force the same failure startup can encounter after loading its
    // registry. Readiness must not silently continue after this scan fails.
    std::fs::remove_dir_all(std::path::Path::new(&repo_dir).join(".jj/working_copy")).unwrap();
    let err = state.snapshot_all_repos().await.unwrap_err();
    assert!(err.to_string().contains("snapshot"), "{err:#}");
}
