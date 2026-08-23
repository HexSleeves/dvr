mod common;
use serde_json::json;
use std::time::{Duration, Instant};

#[tokio::test]
async fn file_change_is_snapshotted_without_any_api_call() {
    let d = common::spawn_daemon().await;
    let repo_dir = common::fixture_repo("/tmp", "bgtest-watch");
    let (st, info) =
        common::req_json(&d.socket, "POST", "/repos", Some(json!({"path": repo_dir}))).await;
    assert_eq!(st, 200);
    let id = info["id"].as_str().unwrap();

    std::fs::write(std::path::Path::new(&repo_dir).join("watched.txt"), "auto").unwrap();

    // Poll op-log-visible state WITHOUT the snapshot-on-read paths: /status and
    // /log snapshot before reporting, so instead poll GET /repos/{id}/file,
    // which reads the store at @ and never snapshots. It answers 200 only once
    // the watcher has snapshotted watched.txt into the working-copy commit.
    // (req_raw, not req_json: the 200 body is raw file bytes, not JSON.)
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let (st, _body) =
            common::req_raw(&d.socket, &format!("/repos/{id}/file?rev=@&path=watched.txt")).await;
        if st == 200 {
            break; // file is in the wc commit => watcher snapshotted it
        }
        assert!(Instant::now() < deadline, "watcher never snapshotted the change");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}
