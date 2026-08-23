//! End-to-end test through the REAL `dvr` binary (CARGO_BIN_EXE_dvr), with an
//! isolated DVR_STATE_DIR under /tmp (unix socket sun_path stays short). The
//! first `dvr register` runs with no daemon: it must auto-start `dvrd` itself —
//! that auto-start is part of what this test asserts.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Kills the auto-started dvrd (pid from `state/dvrd.pid`) and removes the state
/// dir. Runs on Drop so cleanup happens even when an assertion panics — no
/// zombie daemons or stale sockets left behind by failing runs.
struct DaemonGuard {
    state: PathBuf,
}

impl Drop for DaemonGuard {
    fn drop(&mut self) {
        if let Ok(pid) = std::fs::read_to_string(self.state.join("dvrd.pid")) {
            let _ = Command::new("kill").arg(pid.trim()).status();
        }
        let _ = std::fs::remove_dir_all(&self.state);
    }
}

fn dvr(args: &[&str], cwd: &Path, state: &Path) -> (bool, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_dvr"))
        .args(args)
        .current_dir(cwd)
        .env("DVR_STATE_DIR", state)
        .output()
        .unwrap();
    (
        out.status.success(),
        format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr)),
    )
}

#[test]
fn register_st_describe_log_via_cli_with_autostart() {
    let state = PathBuf::from(format!("/tmp/dvr-cli-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&state);
    std::fs::create_dir_all(&state).unwrap();
    let _guard = DaemonGuard { state: state.clone() };

    let repo = state.join("proj");
    std::fs::create_dir(&repo).unwrap();
    let git = |args: &[&str]| {
        let out = Command::new("git")
            .args(["-c", "user.name=t", "-c", "user.email=t@t"])
            .args(args)
            .current_dir(&repo)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    };
    git(&["init", "-b", "main"]);
    std::fs::write(repo.join("README.md"), "x").unwrap();
    git(&["add", "-A"]);
    git(&["commit", "-m", "init"]);

    // No dvrd is running against this state dir: register must auto-start it.
    let (ok, out) = dvr(&["register"], &repo, &state);
    assert!(ok, "{out}");
    assert!(
        state.join("dvrd.pid").is_file(),
        "auto-started daemon must write its pidfile: {out}"
    );

    std::fs::write(repo.join("w.txt"), "1").unwrap();
    let (ok, out) = dvr(&["st"], &repo, &state);
    assert!(ok && out.contains("w.txt"), "{out}");

    let (ok, out) = dvr(&["describe", "-m", "add w"], &repo, &state);
    assert!(ok, "{out}");
    let (ok, out) = dvr(&["log", "-n", "5"], &repo, &state);
    assert!(ok && out.contains("add w"), "{out}");
    assert!(out.contains('@'), "log must mark the working-copy row: {out}");

    // `dvr file` streams raw contents to stdout.
    let (ok, out) = dvr(&["file", "-r", "@", "w.txt"], &repo, &state);
    assert!(ok && out == "1", "{out:?}");

    // Errors render as `error: <message>` (+ hint line) and exit non-zero.
    let outside = state.join("elsewhere");
    std::fs::create_dir(&outside).unwrap();
    let (ok, out) = dvr(&["st"], &outside, &state);
    assert!(!ok, "st outside any registered repo must fail: {out}");
    assert!(out.contains("error:"), "{out}");
}
