use std::path::Path;
use std::process::Command;

pub fn fixture_git_repo(dir: &Path) {
    let git = |args: &[&str]| {
        let st = Command::new("git")
            .args(["-c", "user.name=t", "-c", "user.email=t@t"])
            .args(args)
            .current_dir(dir)
            .status()
            .unwrap();
        assert!(st.success(), "git {args:?} failed");
    };
    git(&["init", "-b", "main"]);
    std::fs::write(dir.join("README.md"), "# fixture\n").unwrap();
    git(&["add", "-A"]);
    git(&["commit", "-m", "init"]);
}
