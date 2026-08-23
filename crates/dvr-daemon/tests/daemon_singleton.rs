//! Daemon singleton: two dvrd's on the same state dir would both rebind the
//! socket and write the same repos' op stores. The second starter must fail
//! fast with a clear error while the first keeps serving.

mod common;
use std::time::Duration;

#[tokio::test]
async fn second_daemon_on_same_state_dir_fails_fast() {
    let d = common::spawn_daemon().await;
    let dir = d.socket.parent().unwrap().to_path_buf();

    let second = tokio::time::timeout(Duration::from_secs(5), dvr_daemon::run_with_dir(dir)).await;
    let err = second
        .expect("second run_with_dir must fail fast, not serve")
        .expect_err("second daemon on the same state dir must refuse to start");
    assert!(err.to_string().contains("already running"), "unclear error: {err:#}");

    // The refusal must not have torn down the first daemon's socket.
    let (st, _) = common::req_json(&d.socket, "GET", "/health", None).await;
    assert_eq!(st, 200, "first daemon must keep serving");
}
