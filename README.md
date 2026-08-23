# dvr

dvr is a jj-powered source-control daemon that removes git ceremony: the working copy is always a commit, every file change is snapshotted automatically, and history is named after the fact with `dvr describe` instead of staged and stashed along the way. A per-machine daemon (`dvrd`) embeds [jj-lib](https://github.com/jj-vcs/jj), owns every registered repo, and serves a JSON API over a unix socket; `dvr` is a thin CLI client over that socket. Registered repos remain plain git repos — editors, build tools, and GitHub flows keep working unchanged. Full design: [docs/superpowers/specs/2026-08-22-bg-design.md](docs/superpowers/specs/2026-08-22-bg-design.md).

## Install

```sh
cargo install --path crates/dvr-cli
```

This installs both `dvr` and `dvrd`. You never start `dvrd` yourself: the first `dvr` command auto-starts it in the background (its output goes to `dvrd.log` in the state dir). `dvr daemon run` runs it in the foreground instead, if you want logs in a terminal.

## Quickstart

```sh
cd ~/src/myproject
dvr register                    # hand this repo to the daemon

# ...edit files. No add, no stash, no WIP commits — the daemon watches the
# tree and snapshots every change into the working-copy commit.

dvr st                          # what changed
dvr describe -m "fix: handle empty input"
dvr push -b fix/empty-input --create
```

The full command set:

| Command | What it does |
| --- | --- |
| `dvr register [path]` | Register a git repo (default: current directory) |
| `dvr st` | Working-copy status of the repo you are in |
| `dvr log -n 20` | Recent changes; `@` marks the working copy |
| `dvr describe -m <msg> [-r <change>]` | Name a change (default: the working copy) |
| `dvr ws new <name> [--dest <dir>] [-r <change>]` | New workspace as a copy-on-write clone (APFS `clonefile` — milliseconds, and `node_modules`/build caches come along) |
| `dvr ws list` | List workspaces |
| `dvr push -b <bookmark> [--remote <name>] [-r <change>] [--create]` | Push a change to an explicit remote bookmark |
| `dvr file -r <rev> <path>` | Print a file's contents at any revision, straight from the store |

Deleting a workspace is just `rm -rf` — every state it ever held is already in the store.

Workspaces are cloned from the default workspace's current files. With `dvr ws new -r X`, those files become a new change whose parent is `X`; dvr does not replace them with `X`'s tree.

## Guardrails

- **Never auto-push.** No background operation ever touches a remote. Only `dvr push` writes to one.
- **Explicit push targets.** Every push states exactly which remote and bookmark it writes to; there is no "push to wherever this tracks". Pushing a bookmark that does not exist on the remote is refused unless you pass `--create`.
- **No implicit branch tracking.** New changes never silently attach themselves to a remote branch.
- **One op-store writer.** Do not run `jj` commands inside registered repos; dvr owns their `.jj` state. Plain `git` commands in the registered root are supported, and moved Git HEADs and refs are imported on the next snapshot.

## State

Daemon state lives in `$DVR_STATE_DIR`, defaulting to `~/.local/state/dvr`: the socket (`dvrd.sock`), pidfile (`dvrd.pid`), daemon log (`dvrd.log`), and the repo registry (`repos.json`). Repo history itself lives in each repo's own `.jj` directory next to `.git` — removing the state dir only forgets which repos are registered, never their history.

Crash safety: on startup the daemon re-scans every registered repo and snapshots any drift *before* serving requests, so edits made while it was down still land in the log.

## v1 limitations

- macOS only — the watcher uses FSEvents and workspaces use APFS `clonefile`.
- No MCP server yet; agents get native tools over the same API in v2.
- No `--track` auto-rebase of workspaces against main; that is v3.
- A cloned workspace keeps a point-in-time copy of `.git` for Git-based tools. That copy is not synchronized in v1 and can become stale until workspace Git export lands.
