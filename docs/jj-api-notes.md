# jj API Reference Notes

## Pin

jj-lib version: **0.44.0** (MSRV 1.89)

## Key APIs

- **Existing repo initialization**: `init_external_git()` — initializes jj backend for existing git repositories.
- **Snapshot flow**: Located in `cli/src/cli_util.rs` (`maybe_snapshot()`) — handles workspace snapshots.
- **Workspace add flow**: Implemented in `cli/src/commands/workspace/add.rs` — manages workspace addition.
- **Push implementation**: Uses git subprocess for push operations.

See `~/.cache/jj-src` for full source reference.
