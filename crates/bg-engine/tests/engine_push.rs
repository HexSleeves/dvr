mod common;
use bg_engine::{settings::make_settings, RepoEngine};
use bg_proto::PushRequest;

async fn setup(dir: &std::path::Path) -> (RepoEngine, std::path::PathBuf) {
    let root = dir.join("repo");
    let remote = dir.join("remote.git");
    std::fs::create_dir(&root).unwrap();
    common::fixture_git_repo_with_remote(&root, &remote);
    let mut e = RepoEngine::open_or_init(&root, make_settings("T", "t@t").unwrap()).await.unwrap();
    std::fs::write(root.join("feature.txt"), "work").unwrap();
    e.snapshot("default").await.unwrap();
    (e, remote)
}

fn remote_heads(remote: &std::path::Path) -> String {
    let out = std::process::Command::new("git")
        .args(["ls-remote", "--heads"])
        .arg(remote)
        .output()
        .unwrap();
    String::from_utf8(out.stdout).unwrap()
}

#[tokio::test]
async fn push_refuses_undescribed_change() {
    let dir = tempfile::tempdir_in("/tmp").unwrap();
    let (mut e, remote) = setup(dir.path()).await;
    let req = PushRequest { change_id: None, remote: "origin".into(), bookmark: "feat/x".into(), create: true };
    let err = e.push("default", &req).await.unwrap_err();
    assert!(err.to_string().contains("describe"), "got: {err:#}");
    assert!(remote_heads(&remote).is_empty(), "guardrail must prevent any remote write");
}

#[tokio::test]
async fn push_refuses_new_bookmark_without_create() {
    let dir = tempfile::tempdir_in("/tmp").unwrap();
    let (mut e, remote) = setup(dir.path()).await;
    e.describe("default", None, "feat: x").await.unwrap();
    let req = PushRequest { change_id: None, remote: "origin".into(), bookmark: "feat/x".into(), create: false };
    let err = e.push("default", &req).await.unwrap_err();
    assert!(err.to_string().contains("create"), "got: {err:#}");
    assert!(remote_heads(&remote).is_empty());
}

#[tokio::test]
async fn push_with_create_lands_exactly_the_named_branch() {
    let dir = tempfile::tempdir_in("/tmp").unwrap();
    let (mut e, remote) = setup(dir.path()).await;
    e.describe("default", None, "feat: x").await.unwrap();
    let req = PushRequest { change_id: None, remote: "origin".into(), bookmark: "feat/x".into(), create: true };
    let resp = e.push("default", &req).await.unwrap();
    assert_eq!(resp.bookmark, "feat/x");
    let heads = remote_heads(&remote);
    assert!(heads.contains("refs/heads/feat/x"), "{heads}");
    assert_eq!(heads.lines().count(), 1, "must push ONLY the named bookmark: {heads}");
}

#[tokio::test]
async fn push_snapshots_edits_before_selecting_working_copy() {
    let dir = tempfile::tempdir_in("/tmp").unwrap();
    let (mut e, remote) = setup(dir.path()).await;
    e.describe("default", None, "feat: fresh push").await.unwrap();
    // No explicit snapshot: an immediate push must not race the watcher's
    // debounce window and publish the previous tree.
    std::fs::write(dir.path().join("repo/late.txt"), "included").unwrap();
    let req = PushRequest {
        change_id: None,
        remote: "origin".into(),
        bookmark: "feat/fresh".into(),
        create: true,
    };
    e.push("default", &req).await.unwrap();

    let out = std::process::Command::new("git")
        .arg("--git-dir")
        .arg(&remote)
        .args(["show", "refs/heads/feat/fresh:late.txt"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "immediate push omitted the unsnapshotted edit: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(out.stdout, b"included");
}

#[test]
fn git_push_appears_only_in_push_rs() {
    // Guardrail enforcement: every production git subprocess site is
    // explicit. Any new site fails until a reviewer verifies and allowlists
    // it; push.rs is the only site allowed to issue arbitrary git commands.
    let workspace = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut grep = std::process::Command::new("grep");
    grep.args(["-Rn", "--include=*.rs", "-F", "Command::new(\"git\")"]);
    for entry in std::fs::read_dir(workspace.join("crates")).unwrap() {
        let src = entry.unwrap().path().join("src");
        if src.is_dir() {
            grep.arg(src);
        }
    }
    let out = grep.output().unwrap();
    assert!(out.status.success(), "guardrail grep failed: {}", String::from_utf8_lossy(&out.stderr));
    let files = String::from_utf8(out.stdout).unwrap();
    let allowlist = [
        (
            "crates/bg-daemon/src/state.rs",
            "Command::new(\"git\").args([\"config\", key])",
        ),
        (
            "crates/bg-engine/src/push.rs",
            "Command::new(\"git\").arg(\"-C\").arg(root).args(args)",
        ),
    ];
    let mut seen = std::collections::BTreeSet::new();
    let mut hit_count = 0;
    for hit in files.lines() {
        hit_count += 1;
        let (path, source) = hit.split_once(':').expect("grep output must contain path and source");
        let relative = std::path::Path::new(path).strip_prefix(&workspace).unwrap();
        let relative = relative.to_string_lossy();
        let allowed = allowlist
            .iter()
            .any(|(allowed_path, fragment)| relative == *allowed_path && source.contains(fragment));
        assert!(allowed, "unreviewed production git subprocess site: {relative}:{source}");
        seen.insert(relative.into_owned());
    }
    assert_eq!(hit_count, allowlist.len(), "guardrail allowlist and grep hits diverged: {files}");
    assert_eq!(seen.len(), allowlist.len(), "guardrail allowlist and grep hits diverged: {files}");
    assert!(seen.contains("crates/bg-engine/src/push.rs"), "guardrail grep went vacuous");
}

#[tokio::test]
async fn push_rejects_glob_bookmark() {
    let dir = tempfile::tempdir_in("/tmp").unwrap();
    let (mut e, remote) = setup(dir.path()).await;
    e.describe("default", None, "feat: x").await.unwrap();
    let req = PushRequest { change_id: None, remote: "origin".into(), bookmark: "*".into(), create: true };
    let err = e.push("default", &req).await.unwrap_err();
    assert!(err.to_string().contains("invalid bookmark"), "got: {err:#}");
    assert!(remote_heads(&remote).is_empty(), "a glob bookmark must never reach git push");
}
