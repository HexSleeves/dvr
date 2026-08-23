pub mod engine;
mod push;
pub mod settings;
mod snapshot;
mod workspace_ops;

pub use engine::RepoEngine;

/// Typed markers wrapped into `anyhow::Error` at construction sites so callers
/// (the daemon's HTTP routes) can map failures to statuses by
/// `err.downcast_ref::<EngineError>()` instead of string-matching messages.
#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    /// A named thing (workspace, revision, path in tree) does not exist.
    #[error("{0}")]
    NotFound(String),
    /// An operation was refused by a bg guardrail.
    #[error("{0}")]
    Guardrail(String),
    /// The request itself is bad (invalid workspace name, occupied
    /// destination, ...).
    #[error("{0}")]
    Invalid(String),
}
