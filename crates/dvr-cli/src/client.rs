//! Unix-socket HTTP client for the dvrd daemon, with transparent auto-start:
//! if nothing answers on `socket_path()`, spawn the `dvrd` binary sitting next
//! to the current executable and wait for `/health`.

use std::path::PathBuf;
use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::Context as _;
use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use hyper::{Method, Request, StatusCode};
use hyper_util::client::legacy::Client as HyperClient;
use hyperlocal::{UnixClientExt, UnixConnector, Uri};

/// A daemon-reported error (`ApiError` JSON), carried as the typed source so
/// `main` can render `error: {message}` plus the hint on its own line.
#[derive(Debug)]
pub struct ApiFailure {
    pub message: String,
    pub hint: Option<String>,
}

impl std::fmt::Display for ApiFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ApiFailure {}

pub struct Client {
    socket: PathBuf,
    http: HyperClient<UnixConnector, Full<Bytes>>,
}

impl Client {
    /// Connects to the daemon on `socket_path()`, auto-starting `dvrd` (the
    /// sibling of the current executable) if nothing is listening yet.
    pub async fn connect() -> anyhow::Result<Client> {
        let client =
            Client { socket: dvr_daemon::socket_path(), http: HyperClient::unix() };
        if client.health().await.is_ok() {
            return Ok(client);
        }
        client.spawn_daemon()?;
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if client.health().await.is_ok() {
                return Ok(client);
            }
            anyhow::ensure!(
                Instant::now() < deadline,
                "daemon did not become healthy within 5s (log: {})",
                dvr_daemon::state_dir().join("dvrd.log").display()
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// Spawns `dvrd` detached, with stdout/stderr appended to
    /// `state_dir()/dvrd.log` so daemon noise never lands on the CLI terminal.
    fn spawn_daemon(&self) -> anyhow::Result<()> {
        let exe = std::env::current_exe().context("cannot locate current executable")?;
        let dvrd = exe
            .parent()
            .context("current executable has no parent directory")?
            .join("dvrd");
        let state = dvr_daemon::state_dir();
        std::fs::create_dir_all(&state)
            .with_context(|| format!("cannot create state dir {}", state.display()))?;
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(state.join("dvrd.log"))?;
        let err_log = log.try_clone()?;
        std::process::Command::new(&dvrd)
            .stdin(Stdio::null())
            .stdout(Stdio::from(log))
            .stderr(Stdio::from(err_log))
            .spawn()
            .with_context(|| format!("failed to start daemon binary {}", dvrd.display()))?;
        Ok(())
    }

    async fn health(&self) -> anyhow::Result<()> {
        let (status, _) = self.request("GET", "/health", None).await?;
        anyhow::ensure!(status == StatusCode::OK, "health returned {status}");
        Ok(())
    }

    /// Sends a JSON request; parses the reply as JSON. Non-2xx replies become
    /// a typed `ApiFailure` built from the daemon's `ApiError` body.
    pub async fn json(
        &self,
        method: &str,
        path: &str,
        body: Option<serde_json::Value>,
    ) -> anyhow::Result<serde_json::Value> {
        let (status, bytes) = self.request(method, path, body).await?;
        if !status.is_success() {
            return Err(api_failure(status, &bytes).into());
        }
        serde_json::from_slice(&bytes)
            .with_context(|| format!("daemon sent invalid JSON for {path}"))
    }

    /// GET returning the raw body — for `/file` (application/octet-stream).
    pub async fn bytes(&self, path: &str) -> anyhow::Result<Vec<u8>> {
        let (status, bytes) = self.request("GET", path, None).await?;
        if !status.is_success() {
            return Err(api_failure(status, &bytes).into());
        }
        Ok(bytes.to_vec())
    }

    async fn request(
        &self,
        method: &str,
        path: &str,
        body: Option<serde_json::Value>,
    ) -> anyhow::Result<(StatusCode, Bytes)> {
        let uri: hyper::Uri = Uri::new(&self.socket, path).into();
        let builder = Request::builder().method(Method::from_bytes(method.as_bytes())?).uri(uri);
        let req = match body {
            Some(v) => builder
                .header("content-type", "application/json")
                .body(Full::new(Bytes::from(serde_json::to_vec(&v)?)))?,
            None => builder.body(Full::default())?,
        };
        let res = self
            .http
            .request(req)
            .await
            .with_context(|| format!("cannot reach daemon at {}", self.socket.display()))?;
        let status = res.status();
        let bytes = res.into_body().collect().await?.to_bytes();
        Ok((status, bytes))
    }
}

fn api_failure(status: StatusCode, bytes: &[u8]) -> ApiFailure {
    match serde_json::from_slice::<dvr_proto::ApiError>(bytes) {
        Ok(err) => ApiFailure { message: err.message, hint: err.hint },
        Err(_) => ApiFailure {
            message: format!("daemon returned {status}: {}", String::from_utf8_lossy(bytes)),
            hint: None,
        },
    }
}

/// Percent-encodes one URI path segment (RFC 3986 unreserved set stays raw),
/// so absolute filesystem paths survive as a single `{id}` route segment.
pub fn encode_segment(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for b in raw.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// The repo identifier the CLI sends: its current directory. The daemon
/// resolves any path inside a registered root to that repo.
pub fn cwd_repo_segment() -> anyhow::Result<String> {
    let cwd = std::env::current_dir().context("cannot determine current directory")?;
    let cwd = cwd
        .to_str()
        .with_context(|| format!("current directory is not valid UTF-8: {}", cwd.display()))?;
    Ok(encode_segment(cwd))
}
