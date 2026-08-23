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

#[test]
fn git_push_appears_only_in_push_rs() {
    // Guardrail enforcement: the string "push" as a git subcommand exists only in push.rs.
    let out = std::process::Command::new("grep")
        .args(["-rn", "--include=*.rs", "-l", "\"push\""])
        .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/src"))
        .output()
        .unwrap();
    let files = String::from_utf8(out.stdout).unwrap();
    for f in files.lines() {
        assert!(f.ends_with("push.rs"), "git push leaked outside push.rs: {f}");
    }
}
