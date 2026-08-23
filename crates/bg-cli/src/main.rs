//! `bg` — thin client over the bgd daemon's unix-socket HTTP API.

mod client;
mod commands;

use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "bg", version, about = "jj-powered repo daemon client")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Register a repo with the daemon (defaults to the current directory)
    Register {
        path: Option<PathBuf>,
    },
    /// Working-copy status of the repo containing the current directory
    St,
    /// Recent changes
    Log {
        /// Maximum number of entries
        #[arg(short = 'n', long = "limit", default_value_t = 20)]
        limit: usize,
    },
    /// Set the description of a change (defaults to the working copy)
    Describe {
        #[arg(short = 'r', long = "rev")]
        change: Option<String>,
        #[arg(short = 'm', long = "message")]
        message: String,
    },
    /// Workspace operations
    Ws {
        #[command(subcommand)]
        cmd: WsCmd,
    },
    /// Push a change to an explicit remote bookmark
    Push {
        #[arg(short = 'b', long = "bookmark")]
        bookmark: String,
        #[arg(long, default_value = "origin")]
        remote: String,
        #[arg(short = 'r', long = "rev")]
        change: Option<String>,
        /// Allow creating the bookmark on the remote
        #[arg(long)]
        create: bool,
    },
    /// Print a file's contents at a revision
    File {
        #[arg(short = 'r', long = "rev")]
        rev: String,
        path: String,
    },
    /// Daemon lifecycle
    Daemon {
        #[command(subcommand)]
        cmd: DaemonCmd,
    },
}

#[derive(Subcommand)]
enum WsCmd {
    /// Create a workspace (default dest: sibling dir `<repo>-<name>`)
    New {
        name: String,
        #[arg(long)]
        dest: Option<PathBuf>,
        #[arg(short = 'r', long = "rev")]
        change: Option<String>,
    },
    /// List workspaces
    List,
}

#[derive(Subcommand)]
enum DaemonCmd {
    /// Run the daemon in the foreground (same as the `bgd` binary)
    Run,
}

async fn dispatch(cmd: Cmd) -> anyhow::Result<()> {
    match cmd {
        Cmd::Register { path } => commands::register(path).await,
        Cmd::St => commands::status().await,
        Cmd::Log { limit } => commands::log(limit).await,
        Cmd::Describe { change, message } => commands::describe(change, message).await,
        Cmd::Ws { cmd: WsCmd::New { name, dest, change } } => {
            commands::workspace_new(name, dest, change).await
        }
        Cmd::Ws { cmd: WsCmd::List } => commands::workspace_list().await,
        Cmd::Push { bookmark, remote, change, create } => {
            commands::push(bookmark, remote, change, create).await
        }
        Cmd::File { rev, path } => commands::file(rev, path).await,
        Cmd::Daemon { cmd: DaemonCmd::Run } => commands::daemon_run().await,
    }
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    if let Err(err) = dispatch(cli.cmd).await {
        // Daemon ApiError bodies carry a message + optional hint; render both.
        if let Some(api) = err.downcast_ref::<client::ApiFailure>() {
            eprintln!("error: {}", api.message);
            if let Some(hint) = &api.hint {
                eprintln!("{hint}");
            }
        } else {
            eprintln!("error: {err:#}");
        }
        std::process::exit(1);
    }
}
