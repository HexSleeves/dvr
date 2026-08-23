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
async fn framework_rejections_are_structured_api_errors() {
    let d = common::spawn_daemon().await;
    let cases = [
        common::req_bytes(&d.socket, "POST", "/repos", "application/json", b"{").await,
        common::req_raw(&d.socket, "/repos/nope/file?rev=@").await,
        common::req_raw(&d.socket, "/not-a-route").await,
        common::req_bytes(&d.socket, "PUT", "/health", "application/json", b"{}").await,
        common::req_bytes(&d.socket, "POST", "/repos", "text/plain", b"{}").await,
    ];
    for (status, body) in cases {
        assert!(status.is_client_error(), "{status}: {}", String::from_utf8_lossy(&body));
        let error: serde_json::Value = serde_json::from_slice(&body).unwrap_or_else(|err| {
            panic!("framework rejection was not ApiError JSON ({status}): {err}: {:?}", String::from_utf8_lossy(&body))
        });
        assert!(error["code"].is_string(), "{status}: {error}");
        assert!(error["message"].is_string(), "{status}: {error}");
        assert!(error.get("hint").is_some(), "{status}: {error}");
    }
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
async fn workspace_create_and_list() {
    let d = common::spawn_daemon().await;
    let repo_dir = common::fixture_repo("/tmp", "bgtest-ws");
    // Default dest is the sibling dir `<root>-<name>`; wipe leftovers up front.
    let ws_dir = std::path::PathBuf::from("/tmp/bgtest-ws-w1");
    let _ = std::fs::remove_dir_all(&ws_dir);

    let (st, info) = common::req_json(&d.socket, "POST", "/repos", Some(json!({"path": repo_dir}))).await;
    assert_eq!(st, 200, "{info}");
    let id = info["id"].as_str().unwrap();

    let (st, ws) = common::req_json(&d.socket, "POST", &format!("/repos/{id}/workspaces"),
        Some(json!({"name": "w1"}))).await;
    assert_eq!(st, 200, "{ws}");
    assert_eq!(ws["name"], "w1");
    assert!(ws_dir.join("README.md").is_file(), "default dest must be the sibling <root>-<name>: {ws}");

    let (st, list) = common::req_json(&d.socket, "GET", &format!("/repos/{id}/workspaces"), None).await;
    assert_eq!(st, 200);
    let names: Vec<&str> = list.as_array().unwrap().iter().map(|w| w["name"].as_str().unwrap()).collect();
    assert_eq!(names, vec!["default", "w1"], "{list}");

    let _ = std::fs::remove_dir_all(&ws_dir);
}

/// Deleting a workspace is documented as "just rm -rf" — a vanished workspace
/// dir must not break /status, /log, or /snapshot for the whole repo (the
/// vanished workspace is skipped; the default workspace keeps snapshotting).
#[tokio::test]
async fn vanished_workspace_dir_does_not_break_repo_routes() {
    let d = common::spawn_daemon().await;
    let repo_dir = common::fixture_repo("/tmp", "bgtest-wsgone");
    let ws_dir = std::path::PathBuf::from("/tmp/bgtest-wsgone-w1");
    let _ = std::fs::remove_dir_all(&ws_dir);

    let (st, info) = common::req_json(&d.socket, "POST", "/repos", Some(json!({"path": repo_dir.clone()}))).await;
    assert_eq!(st, 200, "{info}");
    let id = info["id"].as_str().unwrap();

    let (st, ws) = common::req_json(&d.socket, "POST", &format!("/repos/{id}/workspaces"),
        Some(json!({"name": "w1"}))).await;
    assert_eq!(st, 200, "{ws}");
    std::fs::remove_dir_all(&ws_dir).unwrap();

    // Every read route must survive the vanished dir...
    let (st, status) = common::req_json(&d.socket, "GET", &format!("/repos/{id}/status"), None).await;
    assert_eq!(st, 200, "status must skip the vanished workspace: {status}");
    let (st, log) = common::req_json(&d.socket, "GET", &format!("/repos/{id}/log"), None).await;
    assert_eq!(st, 200, "{log}");

    // ...and the default workspace must still snapshot.
    std::fs::write(std::path::Path::new(&repo_dir).join("after-rm.rs"), "fn a(){}").unwrap();
    let (st, r) = common::req_json(&d.socket, "POST", &format!("/repos/{id}/snapshot"), None).await;
    assert_eq!(st, 200, "{r}");
    assert_eq!(r["changed"], true, "default must still snapshot: {r}");

    // The vanished workspace is still LISTED (its changes live in the store;
    // only snapshotting skips it).
    let (st, list) = common::req_json(&d.socket, "GET", &format!("/repos/{id}/workspaces"), None).await;
    assert_eq!(st, 200);
    let names: Vec<&str> = list.as_array().unwrap().iter().map(|w| w["name"].as_str().unwrap()).collect();
    assert_eq!(names, vec!["default", "w1"], "{list}");
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

#[tokio::test]
async fn push_route_guards_then_lands_named_branch() {
    let d = common::spawn_daemon().await;
    let repo_dir = common::fixture_repo("/tmp", "bgtest-push");
    // A bare remote next to the fixture repo, registered as `origin`.
    let remote = format!("{repo_dir}-remote.git");
    let _ = std::fs::remove_dir_all(&remote);
    let git = |args: &[&str]| {
        let out = std::process::Command::new("git").args(args).output().unwrap();
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    };
    git(&["init", "--bare", &remote]);
    git(&["-C", &repo_dir, "remote", "add", "origin", &remote]);

    let (st, info) = common::req_json(&d.socket, "POST", "/repos", Some(json!({"path": repo_dir}))).await;
    assert_eq!(st, 200, "{info}");
    let id = info["id"].as_str().unwrap().to_string();

    let (st, _) = common::req_json(&d.socket, "POST", &format!("/repos/{id}/describe"),
        Some(json!({"workspace": null, "change_id": null, "message": "feat: push me"}))).await;
    assert_eq!(st, 200);

    // Guardrail: first push to a branch that doesn't exist on the remote
    // requires create=true — refused as 403 guardrail_refused.
    let (st, err) = common::req_json(&d.socket, "POST", &format!("/repos/{id}/push"),
        Some(json!({"change_id": null, "remote": "origin", "bookmark": "feat/x", "create": false}))).await;
    assert_eq!(st, 403, "{err}");
    assert_eq!(err["code"], "guardrail_refused", "{err}");
    assert!(err["message"].as_str().unwrap().contains("create"), "{err}");

    // With create=true the push lands and the response states the destination.
    let (st, resp) = common::req_json(&d.socket, "POST", &format!("/repos/{id}/push"),
        Some(json!({"change_id": null, "remote": "origin", "bookmark": "feat/x", "create": true}))).await;
    assert_eq!(st, 200, "{resp}");
    assert_eq!(resp["remote"], "origin");
    assert_eq!(resp["bookmark"], "feat/x");
    assert!(!resp["commit_id"].as_str().unwrap().is_empty());

    let heads = std::process::Command::new("git").args(["ls-remote", "--heads", &remote]).output().unwrap();
    let heads = String::from_utf8(heads.stdout).unwrap();
    assert!(heads.contains("refs/heads/feat/x"), "{heads}");

    let _ = std::fs::remove_dir_all(&remote);
}
