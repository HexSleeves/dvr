use std::path::Path;
use std::process::Command;

pub fn fixture_git_repo(dir: &Path) {
    let git = |args: &[&str]| {
        // .output() (not .status()) so git noise doesn't pollute test output.
        let out = Command::new("git")
            .args(["-c", "user.name=t", "-c", "user.email=t@t"])
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?} failed: {}", String::from_utf8_lossy(&out.stderr));
    };
    git(&["init", "-b", "main"]);
    std::fs::write(dir.join("README.md"), "# fixture\n").unwrap();
    git(&["add", "-A"]);
    git(&["commit", "-m", "init"]);
}

/// `fixture_git_repo(dir)` plus a `--bare` repo at `remote_dir`, registered
/// as the `origin` remote of `dir`.
#[allow(dead_code)] // each test binary compiles common/ separately; only engine_push uses this
pub fn fixture_git_repo_with_remote(dir: &Path, remote_dir: &Path) {
    fixture_git_repo(dir);
    let bare = Command::new("git").arg("init").arg("--bare").arg(remote_dir).output().unwrap();
    assert!(bare.status.success(), "git init --bare failed: {}", String::from_utf8_lossy(&bare.stderr));
    let add = Command::new("git")
        .arg("-C")
        .arg(dir)
        .arg("remote")
        .arg("add")
        .arg("origin")
        .arg(remote_dir)
        .output()
        .unwrap();
    assert!(add.status.success(), "git remote add failed: {}", String::from_utf8_lossy(&add.stderr));
}
