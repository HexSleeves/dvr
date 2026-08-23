mod common;
use serde_json::json;

#[tokio::test]
async fn register_status_describe_roundtrip() {
    let d = common::spawn_daemon().await;
    let repo_dir = common::fixture_repo("/tmp", "bgtest-roundtrip"); // fixture under /tmp, short path
    let (st, info) = common::req_json(&d.socket, "POST", "/repos", Some(json!({"path": repo_dir}))).await;
    assert_eq!(st, 200, "{info}");
    let id = info["id"].as_str().unwrap().to_string();

    std::fs::write(std::path::Path::new(&repo_dir).join("new.rs"), "fn x(){}").unwrap();
    let (st, status) = common::req_json(&d.socket, "GET", &format!("/repos/{id}/status"), None).await;
    assert_eq!(st, 200);
    let files = status["workspaces"][0]["changed_files"].as_array().unwrap();
    assert!(files.iter().any(|f| f["path"] == "new.rs"), "status must auto-snapshot: {status}");

    let (st, entry) = common::req_json(&d.socket, "POST", &format!("/repos/{id}/describe"),
        Some(json!({"workspace": null, "change_id": null, "message": "wip: new.rs"}))).await;
    assert_eq!(st, 200);
    assert_eq!(entry["description"], "wip: new.rs");
}

#[tokio::test]
async fn unknown_repo_is_404_with_api_error() {
    let d = common::spawn_daemon().await;
    let (st, err) = common::req_json(&d.socket, "GET", "/repos/nope/status", None).await;
    assert_eq!(st, 404);
    assert_eq!(err["code"], "not_found");
}

#[tokio::test]
async fn health_reports_version() {
    let d = common::spawn_daemon().await;
    let (st, v) = common::req_json(&d.socket, "GET", "/health", None).await;
    assert_eq!(st, 200);
    assert_eq!(v["version"], env!("CARGO_PKG_VERSION"));
}

#[tokio::test]
async fn register_dedups_ids_and_is_idempotent() {
    let d = common::spawn_daemon().await;
    let base = format!("/tmp/bg-fx-{}", std::process::id());
    let a = common::fixture_repo(&format!("{base}/a"), "twin");
    let b = common::fixture_repo(&format!("{base}/b"), "twin");

    let (st, ia) = common::req_json(&d.socket, "POST", "/repos", Some(json!({"path": a}))).await;
    assert_eq!(st, 200, "{ia}");
    assert_eq!(ia["id"], "twin");

    let (st, ib) = common::req_json(&d.socket, "POST", "/repos", Some(json!({"path": b}))).await;
    assert_eq!(st, 200, "{ib}");
    assert_eq!(ib["id"], "twin-2", "same dir name must dedup: {ib}");

    // Re-registering the same root returns the existing registration.
    let (st, ia2) = common::req_json(&d.socket, "POST", "/repos", Some(json!({"path": a}))).await;
    assert_eq!(st, 200);
    assert_eq!(ia2["id"], "twin");

    let (st, list) = common::req_json(&d.socket, "GET", "/repos", None).await;
    assert_eq!(st, 200);
    let ids: Vec<&str> = list.as_array().unwrap().iter().map(|r| r["id"].as_str().unwrap()).collect();
    assert_eq!(ids, vec!["twin", "twin-2"], "{list}");

    let _ = std::fs::remove_dir_all(&base);
}

#[tokio::test]
async fn register_rejects_non_git_dir() {
    let d = common::spawn_daemon().await;
    let dir = format!("/tmp/bg-nogit-{}", std::process::id());
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let (st, err) = common::req_json(&d.socket, "POST", "/repos", Some(json!({"path": dir.clone()}))).await;
    assert_eq!(st, 400, "{err}");
    assert_eq!(err["code"], "invalid_request");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn resolve_accepts_path_inside_repo() {
    let d = common::spawn_daemon().await;
    let repo_dir = common::fixture_repo("/tmp", "bgtest-bypath");
    let (st, info) = common::req_json(&d.socket, "POST", "/repos", Some(json!({"path": repo_dir.clone()}))).await;
    assert_eq!(st, 200, "{info}");

    // The CLI sends its cwd — any path inside a registered root must resolve.
    let sub = std::path::Path::new(&repo_dir).join("src");
    std::fs::create_dir_all(&sub).unwrap();
    let encoded = sub.to_str().unwrap().replace('/', "%2F");
    let (st, status) = common::req_json(&d.socket, "GET", &format!("/repos/{encoded}/status"), None).await;
    assert_eq!(st, 200, "{status}");
    assert_eq!(status["repo"]["id"], info["id"]);
}

#[tokio::test]
async fn log_snapshots_and_reflects_describe() {
    let d = common::spawn_daemon().await;
    let repo_dir = common::fixture_repo("/tmp", "bgtest-log");
    let (st, info) = common::req_json(&d.socket, "POST", "/repos", Some(json!({"path": repo_dir}))).await;
    assert_eq!(st, 200, "{info}");
    let id = info["id"].as_str().unwrap();

    let (st, _) = common::req_json(&d.socket, "POST", &format!("/repos/{id}/describe"),
        Some(json!({"workspace": null, "change_id": null, "message": "hello"}))).await;
    assert_eq!(st, 200);

    let (st, log) = common::req_json(&d.socket, "GET", &format!("/repos/{id}/log?limit=1"), None).await;
    assert_eq!(st, 200);
    let entries = log.as_array().unwrap();
    assert_eq!(entries.len(), 1, "limit must apply: {log}");
    assert_eq!(entries[0]["description"], "hello");
    assert_eq!(entries[0]["is_working_copy"], true);
}

#[tokio::test]
async fn snapshot_route_reports_changed() {
    let d = common::spawn_daemon().await;
    let repo_dir = common::fixture_repo("/tmp", "bgtest-snap");
    let (st, info) = common::req_json(&d.socket, "POST", "/repos", Some(json!({"path": repo_dir.clone()}))).await;
    assert_eq!(st, 200, "{info}");
    let id = info["id"].as_str().unwrap();

    std::fs::write(std::path::Path::new(&repo_dir).join("dirty.rs"), "fn d(){}").unwrap();
    let (st, r) = common::req_json(&d.socket, "POST", &format!("/repos/{id}/snapshot"), None).await;
    assert_eq!(st, 200);
    assert_eq!(r["changed"], true, "{r}");

    let (st, r) = common::req_json(&d.socket, "POST", &format!("/repos/{id}/snapshot"), None).await;
    assert_eq!(st, 200);
    assert_eq!(r["changed"], false, "second snapshot with no edits: {r}");
}

#[tokio::test]
async fn file_returns_raw_bytes_and_404_on_missing_path() {
    let d = common::spawn_daemon().await;
    let repo_dir = common::fixture_repo("/tmp", "bgtest-file");
    let (st, info) = common::req_json(&d.socket, "POST", "/repos", Some(json!({"path": repo_dir}))).await;
    assert_eq!(st, 200, "{info}");
    let id = info["id"].as_str().unwrap();

    let (st, body) = common::req_raw(&d.socket, &format!("/repos/{id}/file?rev=@&path=README.md")).await;
    assert_eq!(st, 200);
    assert_eq!(body, b"# fixture\n");

    let (st, body) = common::req_raw(&d.socket, &format!("/repos/{id}/file?rev=@&path=missing.txt")).await;
    assert_eq!(st, 404);
    let err: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(err["code"], "not_found", "{err}");
}
