use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use hyper::{Method, Request, StatusCode};
use hyper_util::client::legacy::Client;
use hyperlocal::{UnixClientExt, UnixConnector, Uri};

pub struct TestDaemon {
    pub socket: PathBuf,
    task: tokio::task::JoinHandle<()>,
    /// `Some` when the daemon owns its state dir (`spawn_daemon`); `None` when
    /// the caller owns it (`spawn_daemon_in`) so it survives daemon restarts.
    _state: Option<TempDirGuard>,
}

impl TestDaemon {
    /// Simulates a daemon crash: aborts the in-process daemon task and removes
    /// its socket. The state dir and the repos' `.jj` dirs survive untouched,
    /// so a later `spawn_daemon_in` on the same state dir is a restart.
    #[allow(dead_code)] // each test binary compiles its own common; used by daemon_restart.rs
    pub async fn shutdown(self) {
        self.task.abort();
        let _ = self.task.await; // wait for the abort so the daemon state is dropped
        let _ = std::fs::remove_file(&self.socket);
    }
}

/// Removes the daemon state dir when the test is done.
pub struct TempDirGuard(PathBuf);

impl TempDirGuard {
    #[allow(dead_code)] // each test binary compiles its own common; used by daemon_restart.rs
    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDirGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

static COUNTER: AtomicUsize = AtomicUsize::new(0);

/// Creates a fresh state dir directly under /tmp (unix socket paths must stay
/// well under the ~100-byte `sun_path` limit). The guard removes it on drop —
/// hold it across daemon restarts within a test.
pub fn fresh_state_dir() -> TempDirGuard {
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = PathBuf::from(format!("/tmp/bg-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    TempDirGuard(dir)
}

/// Spawns `bg_daemon::run_with_dir` on a fresh state dir and waits until
/// `/health` answers. No env mutation — parallel-safe.
#[allow(dead_code)] // each test binary compiles its own common; daemon_restart.rs doesn't use it
pub async fn spawn_daemon() -> TestDaemon {
    let state = fresh_state_dir();
    let mut daemon = spawn_daemon_in(&state.0).await;
    daemon._state = Some(state);
    daemon
}

/// Spawns a daemon on an existing state dir the CALLER owns — the state
/// survives `shutdown`, so tests can stop and restart "the same" daemon.
pub async fn spawn_daemon_in(dir: &Path) -> TestDaemon {
    let socket = dir.join("bgd.sock");
    let run_dir = dir.to_path_buf();
    let task = tokio::spawn(async move {
        if let Err(err) = bg_daemon::run_with_dir(run_dir).await {
            eprintln!("daemon exited with error: {err:#}");
        }
    });

    for _ in 0..200 {
        if let Ok((StatusCode::OK, _)) = try_req(&socket, "GET", "/health", None).await {
            return TestDaemon { socket, task, _state: None };
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("daemon did not become healthy within 5s ({})", socket.display());
}

async fn try_req(
    socket: &Path,
    method: &str,
    path: &str,
    body: Option<serde_json::Value>,
) -> anyhow::Result<(StatusCode, Bytes)> {
    let client: Client<UnixConnector, Full<Bytes>> = Client::unix();
    let uri: hyper::Uri = Uri::new(socket, path).into();
    let builder = Request::builder().method(Method::from_bytes(method.as_bytes())?).uri(uri);
    let req = match body {
        Some(v) => builder
            .header("content-type", "application/json")
            .body(Full::new(Bytes::from(serde_json::to_vec(&v)?)))?,
        None => builder.body(Full::default())?,
    };
    let res = client.request(req).await?;
    let status = res.status();
    let bytes = res.into_body().collect().await?.to_bytes();
    Ok((status, bytes))
}

/// Sends an arbitrary request body and returns the unparsed response. Used to
/// exercise framework-level rejections that cannot be expressed as a valid
/// serde_json::Value.
#[allow(dead_code)] // each test binary compiles its own common module
pub async fn req_bytes(
    socket: &Path,
    method: &str,
    path: &str,
    content_type: &str,
    body: &[u8],
) -> (StatusCode, Vec<u8>) {
    let client: Client<UnixConnector, Full<Bytes>> = Client::unix();
    let uri: hyper::Uri = Uri::new(socket, path).into();
    let req = Request::builder()
        .method(Method::from_bytes(method.as_bytes()).unwrap())
        .uri(uri)
        .header("content-type", content_type)
        .body(Full::new(Bytes::copy_from_slice(body)))
        .unwrap();
    let res = client.request(req).await.expect("request failed");
    let status = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    (status, bytes.to_vec())
}

/// Sends a request over the daemon's unix socket and parses the JSON reply.
pub async fn req_json(
    socket: &Path,
    method: &str,
    path: &str,
    body: Option<serde_json::Value>,
) -> (StatusCode, serde_json::Value) {
    let (status, bytes) = try_req(socket, method, path, body).await.expect("request failed");
    let value = serde_json::from_slice(&bytes).unwrap_or_else(|err| {
        panic!("non-JSON response ({status}): {err}: {:?}", String::from_utf8_lossy(&bytes))
    });
    (status, value)
}

/// GET returning the raw body — for the /file route (application/octet-stream).
#[allow(dead_code)] // each test binary compiles its own common module
pub async fn req_raw(socket: &Path, path: &str) -> (StatusCode, Vec<u8>) {
    let (status, bytes) = try_req(socket, "GET", path, None).await.expect("request failed");
    (status, bytes.to_vec())
}

/// Creates a one-commit git repo at `base/name` (wiping any previous run's
/// leftovers). Duplicated from bg-engine's test fixture — test helpers aren't
/// shared across packages.
#[allow(dead_code)] // each test binary compiles its own common module
pub fn fixture_repo(base: &str, name: &str) -> String {
    let dir = Path::new(base).join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let git = |args: &[&str]| {
        // .output() (not .status()) so git noise doesn't pollute test output.
        let out = Command::new("git")
            .args(["-c", "user.name=t", "-c", "user.email=t@t"])
            .args(args)
            .current_dir(&dir)
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?} failed: {}", String::from_utf8_lossy(&out.stderr));
    };
    git(&["init", "-b", "main"]);
    std::fs::write(dir.join("README.md"), "# fixture\n").unwrap();
    git(&["add", "-A"]);
    git(&["commit", "-m", "init"]);
    dir.to_string_lossy().into_owned()
}
