//! One function per `bg` subcommand: build a `Client`, hit one daemon
//! endpoint, render with plain `println!` (no colors in v1). All repo access
//! goes through the daemon API — the CLI never touches git or jj itself.

use std::io::Write as _;
use std::path::PathBuf;

use anyhow::Context as _;
use bg_proto::{ChangeKind, LogEntry, PushResponse, RepoInfo, StatusResponse, WorkspaceInfo};
use serde_json::json;

use crate::client::{Client, cwd_repo_segment};

fn describe_or_placeholder(description: &str) -> &str {
    let d = description.trim();
    if d.is_empty() { "(no description)" } else { d }
}

/// First line only: log/status rows are one line per change.
fn summary(description: &str) -> &str {
    describe_or_placeholder(description).lines().next().unwrap_or("(no description)")
}

pub async fn register(path: Option<PathBuf>) -> anyhow::Result<()> {
    let path = path.unwrap_or_else(|| PathBuf::from("."));
    let abs = path
        .canonicalize()
        .with_context(|| format!("cannot resolve path {}", path.display()))?;
    let client = Client::connect().await?;
    let info: RepoInfo =
        serde_json::from_value(client.json("POST", "/repos", Some(json!({ "path": abs }))).await?)?;
    println!("registered {} at {}", info.id.0, info.root.display());
    Ok(())
}

pub async fn status() -> anyhow::Result<()> {
    let client = Client::connect().await?;
    let seg = cwd_repo_segment()?;
    let resp: StatusResponse =
        serde_json::from_value(client.json("GET", &format!("/repos/{seg}/status"), None).await?)?;
    for ws in &resp.workspaces {
        println!("{} {}", ws.info.change_id, summary(&ws.info.description));
        for file in &ws.changed_files {
            let kind = match file.kind {
                ChangeKind::Added => 'A',
                ChangeKind::Modified => 'M',
                ChangeKind::Removed => 'D',
            };
            println!("  {kind} {}", file.path);
        }
    }
    Ok(())
}

pub async fn log(limit: usize) -> anyhow::Result<()> {
    let client = Client::connect().await?;
    let seg = cwd_repo_segment()?;
    let entries: Vec<LogEntry> = serde_json::from_value(
        client.json("GET", &format!("/repos/{seg}/log?limit={limit}"), None).await?,
    )?;
    for e in &entries {
        let marker = if e.is_working_copy { '@' } else { ' ' };
        let mut parts = vec![e.change_id.clone()];
        if !e.bookmarks.is_empty() {
            parts.push(e.bookmarks.join(" "));
        }
        parts.push(summary(&e.description).to_string());
        println!("{marker} {}", parts.join(" "));
    }
    Ok(())
}

pub async fn describe(change: Option<String>, message: String) -> anyhow::Result<()> {
    let client = Client::connect().await?;
    let seg = cwd_repo_segment()?;
    let entry: LogEntry = serde_json::from_value(
        client
            .json(
                "POST",
                &format!("/repos/{seg}/describe"),
                Some(json!({ "workspace": null, "change_id": change, "message": message })),
            )
            .await?,
    )?;
    println!("{} {}", entry.change_id, summary(&entry.description));
    Ok(())
}

pub async fn workspace_new(
    name: String,
    dest: Option<PathBuf>,
    change: Option<String>,
) -> anyhow::Result<()> {
    // The daemon only accepts absolute destinations; resolve relative ones
    // against the CLI's cwd where the user typed them.
    let dest = match dest {
        Some(d) if d.is_relative() => Some(std::env::current_dir()?.join(d)),
        other => other,
    };
    let client = Client::connect().await?;
    let seg = cwd_repo_segment()?;
    let ws: WorkspaceInfo = serde_json::from_value(
        client
            .json(
                "POST",
                &format!("/repos/{seg}/workspaces"),
                Some(json!({ "name": name, "dest": dest, "at_change": change })),
            )
            .await?,
    )?;
    println!("created workspace {} at {}", ws.name, ws.path.display());
    Ok(())
}

pub async fn workspace_list() -> anyhow::Result<()> {
    let client = Client::connect().await?;
    let seg = cwd_repo_segment()?;
    let list: Vec<WorkspaceInfo> = serde_json::from_value(
        client.json("GET", &format!("/repos/{seg}/workspaces"), None).await?,
    )?;
    for ws in &list {
        println!(
            "{} {} {} {}",
            ws.name,
            ws.path.display(),
            ws.change_id,
            summary(&ws.description)
        );
    }
    Ok(())
}

pub async fn push(
    bookmark: String,
    remote: String,
    change: Option<String>,
    create: bool,
) -> anyhow::Result<()> {
    let client = Client::connect().await?;
    let seg = cwd_repo_segment()?;
    let resp: PushResponse = serde_json::from_value(
        client
            .json(
                "POST",
                &format!("/repos/{seg}/push"),
                Some(json!({
                    "change_id": change,
                    "remote": remote,
                    "bookmark": bookmark,
                    "create": create,
                })),
            )
            .await?,
    )?;
    // Spec §5: push always says exactly where it went.
    println!("pushed {} to {}/{}", resp.commit_id, resp.remote, resp.bookmark);
    Ok(())
}

pub async fn file(rev: String, path: String) -> anyhow::Result<()> {
    let client = Client::connect().await?;
    let seg = cwd_repo_segment()?;
    let bytes = client
        .bytes(&format!(
            "/repos/{seg}/file?rev={}&path={}",
            crate::client::encode_segment(&rev),
            crate::client::encode_segment(&path),
        ))
        .await?;
    let mut stdout = std::io::stdout().lock();
    stdout.write_all(&bytes)?;
    stdout.flush()?;
    Ok(())
}

pub async fn daemon_run() -> anyhow::Result<()> {
    bg_daemon::run().await
}
