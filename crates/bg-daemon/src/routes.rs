use axum::body::to_bytes;
use axum::extract::{Path, Query, State};
use axum::http::{StatusCode, header};
use axum::middleware;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use bg_proto::{
    ApiError, DescribeRequest, ErrorCode, FileQuery, LogEntry, NewWorkspaceRequest, PushRequest,
    PushResponse, RegisterRequest, RepoInfo, StatusResponse, WorkspaceInfo,
};
use serde_json::json;

use crate::state::{DaemonState, RegisterError, run_engine};

pub fn router(state: DaemonState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/repos", post(register).get(list))
        .route("/repos/{id}/status", get(status))
        .route("/repos/{id}/log", get(log))
        .route("/repos/{id}/describe", post(describe))
        .route("/repos/{id}/snapshot", post(snapshot))
        .route("/repos/{id}/workspaces", post(workspace_new).get(workspace_list))
        .route("/repos/{id}/push", post(push))
        .route("/repos/{id}/file", get(file))
        .with_state(state)
        .layer(middleware::map_response(structure_framework_errors))
}

/// Axum extractor, method, and fallback rejections bypass handler return
/// types. Normalize those framework responses so every daemon API error still
/// honors the structured `ApiError` contract. Handler-produced JSON errors
/// pass through unchanged.
async fn structure_framework_errors(response: Response) -> Response {
    let status = response.status();
    if !status.is_client_error() && !status.is_server_error() {
        return response;
    }
    let is_json = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.starts_with("application/json"));
    if is_json {
        return response;
    }

    let allow = response.headers().get(header::ALLOW).cloned();
    let (_, body) = response.into_parts();
    let bytes = to_bytes(body, 64 * 1024).await.unwrap_or_default();
    let body_message = String::from_utf8_lossy(&bytes).trim().to_string();
    let message = if body_message.is_empty() {
        status.canonical_reason().unwrap_or("request failed").to_string()
    } else {
        body_message
    };
    let code = if status == StatusCode::NOT_FOUND {
        ErrorCode::NotFound
    } else if status.is_client_error() {
        ErrorCode::InvalidRequest
    } else {
        ErrorCode::Internal
    };
    let mut structured = (status, Json(ApiError { code, message, hint: None })).into_response();
    if let Some(allow) = allow {
        structured.headers_mut().insert(header::ALLOW, allow);
    }
    structured
}

/// An `ApiError` plus the HTTP status it travels with. Every handler error
/// funnels through this so failures are always JSON `ApiError` bodies.
pub struct ApiFailure {
    status: StatusCode,
    error: ApiError,
}

impl ApiFailure {
    fn new(status: StatusCode, code: ErrorCode, message: impl Into<String>) -> Self {
        Self { status, error: ApiError { code, message: message.into(), hint: None } }
    }

    fn unknown_repo(id_or_path: &str) -> Self {
        let mut f = Self::new(
            StatusCode::NOT_FOUND,
            ErrorCode::NotFound,
            format!("unknown repo: {id_or_path}"),
        );
        f.error.hint = Some("register the repo first via POST /repos".to_string());
        f
    }

    fn invalid_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, ErrorCode::InvalidRequest, message)
    }

    fn internal(err: anyhow::Error) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, ErrorCode::Internal, format!("{err:#}"))
    }

    /// Maps engine failures by downcasting the typed `EngineError` markers —
    /// never by matching message strings.
    fn from_engine(err: anyhow::Error) -> Self {
        match err.downcast_ref::<bg_engine::EngineError>() {
            Some(bg_engine::EngineError::NotFound(_)) => {
                Self::new(StatusCode::NOT_FOUND, ErrorCode::NotFound, format!("{err:#}"))
            }
            Some(bg_engine::EngineError::Conflict(_)) => {
                Self::new(StatusCode::CONFLICT, ErrorCode::Conflict, format!("{err:#}"))
            }
            Some(bg_engine::EngineError::Guardrail(_)) => Self::new(
                StatusCode::FORBIDDEN,
                ErrorCode::GuardrailRefused,
                format!("{err:#}"),
            ),
            Some(bg_engine::EngineError::Invalid(_)) => Self::new(
                StatusCode::BAD_REQUEST,
                ErrorCode::InvalidRequest,
                format!("{err:#}"),
            ),
            None => Self::internal(err),
        }
    }
}

impl IntoResponse for ApiFailure {
    fn into_response(self) -> Response {
        (self.status, Json(self.error)).into_response()
    }
}

async fn health() -> Json<serde_json::Value> {
    Json(json!({ "version": env!("CARGO_PKG_VERSION") }))
}

async fn register(
    State(st): State<DaemonState>,
    Json(req): Json<RegisterRequest>,
) -> Result<Json<RepoInfo>, ApiFailure> {
    match st.register(&req.path).await {
        Ok(info) => Ok(Json(info)),
        Err(RegisterError::Invalid(msg)) => Err(ApiFailure::invalid_request(msg)),
        Err(RegisterError::Internal(err)) => Err(ApiFailure::internal(err)),
    }
}

async fn list(State(st): State<DaemonState>) -> Json<Vec<RepoInfo>> {
    Json(st.list().await)
}

async fn status(
    State(st): State<DaemonState>,
    Path(id): Path<String>,
) -> Result<Json<StatusResponse>, ApiFailure> {
    let (info, handle) = st.resolve(&id).await.ok_or_else(|| ApiFailure::unknown_repo(&id))?;
    let resp = run_engine(move || async move {
        let mut engine = handle.lock().await;
        // Status is always fresh: snapshot before reporting.
        engine.snapshot_all().await?;
        engine.status(&info.id.0)
    })
    .await
    .map_err(ApiFailure::from_engine)?;
    Ok(Json(resp))
}

#[derive(serde::Deserialize)]
struct LogQuery {
    #[serde(default = "default_log_limit")]
    limit: usize,
}

fn default_log_limit() -> usize {
    50
}

async fn log(
    State(st): State<DaemonState>,
    Path(id): Path<String>,
    Query(q): Query<LogQuery>,
) -> Result<Json<Vec<LogEntry>>, ApiFailure> {
    let (_, handle) = st.resolve(&id).await.ok_or_else(|| ApiFailure::unknown_repo(&id))?;
    let entries = run_engine(move || async move {
        let mut engine = handle.lock().await;
        // Log is always fresh: snapshot before reporting.
        engine.snapshot_all().await?;
        engine.log(q.limit)
    })
    .await
    .map_err(ApiFailure::from_engine)?;
    Ok(Json(entries))
}

async fn describe(
    State(st): State<DaemonState>,
    Path(id): Path<String>,
    Json(req): Json<DescribeRequest>,
) -> Result<Json<LogEntry>, ApiFailure> {
    let (_, handle) = st.resolve(&id).await.ok_or_else(|| ApiFailure::unknown_repo(&id))?;
    let entry = run_engine(move || async move {
        let mut engine = handle.lock().await;
        let ws = req.workspace.as_deref().unwrap_or("default");
        engine.describe(ws, req.change_id.as_deref(), &req.message).await
    })
    .await
    .map_err(ApiFailure::from_engine)?;
    Ok(Json(entry))
}

async fn snapshot(
    State(st): State<DaemonState>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiFailure> {
    let (_, handle) = st.resolve(&id).await.ok_or_else(|| ApiFailure::unknown_repo(&id))?;
    let changed = run_engine(move || async move { handle.lock().await.snapshot_all().await })
        .await
        .map_err(ApiFailure::from_engine)?;
    Ok(Json(json!({ "changed": changed })))
}

/// POST /repos/{id}/workspaces — materializes a new workspace as a CoW clone
/// of the repo root. Default `dest` is the sibling dir `<root>-<name>`. The
/// new workspace dir joins the watcher so it auto-snapshots like the root.
async fn workspace_new(
    State(st): State<DaemonState>,
    Path(id): Path<String>,
    Json(req): Json<NewWorkspaceRequest>,
) -> Result<Json<WorkspaceInfo>, ApiFailure> {
    let (info, handle) = st.resolve(&id).await.ok_or_else(|| ApiFailure::unknown_repo(&id))?;
    // The engine validates existence/nesting lexically, so the daemon only
    // accepts absolute, `..`-free destinations.
    if let Some(dest) = &req.dest {
        let laden = dest.components().any(|c| c == std::path::Component::ParentDir);
        if !dest.is_absolute() || laden {
            return Err(ApiFailure::invalid_request(format!(
                "dest must be an absolute path without ..: {}",
                dest.display()
            )));
        }
    }
    let dest = match req.dest {
        Some(dest) => dest,
        None => {
            let file_name = info.root.file_name().and_then(|n| n.to_str()).ok_or_else(|| {
                ApiFailure::invalid_request(format!(
                    "cannot derive a default dest from {}",
                    info.root.display()
                ))
            })?;
            let parent = info.root.parent().ok_or_else(|| {
                ApiFailure::invalid_request(format!("repo root {} has no parent dir", info.root.display()))
            })?;
            parent.join(format!("{file_name}-{}", req.name))
        }
    };
    let ws = run_engine(move || async move {
        let mut engine = handle.lock().await;
        engine.add_workspace(&req.name, &dest, req.at_change.as_deref()).await
    })
    .await
    .map_err(ApiFailure::from_engine)?;
    st.watch_root(info.id, ws.path.clone());
    Ok(Json(ws))
}

/// GET /repos/{id}/workspaces
async fn workspace_list(
    State(st): State<DaemonState>,
    Path(id): Path<String>,
) -> Result<Json<Vec<WorkspaceInfo>>, ApiFailure> {
    let (_, handle) = st.resolve(&id).await.ok_or_else(|| ApiFailure::unknown_repo(&id))?;
    // Sync accessor; no jj future is awaited, so no run_engine detour needed.
    let engine = handle.lock().await;
    Ok(Json(engine.list_workspaces()))
}

/// POST /repos/{id}/push — thin passthrough to `RepoEngine::push`. All push
/// guardrails (explicit remote+bookmark, described change, create=true for a
/// new remote branch) live in the engine; refusals arrive here as
/// `EngineError::Guardrail` and map to 403 guardrail_refused.
async fn push(
    State(st): State<DaemonState>,
    Path(id): Path<String>,
    Json(req): Json<PushRequest>,
) -> Result<Json<PushResponse>, ApiFailure> {
    let (_, handle) = st.resolve(&id).await.ok_or_else(|| ApiFailure::unknown_repo(&id))?;
    let resp = run_engine(move || async move {
        let mut engine = handle.lock().await;
        engine.push("default", &req).await
    })
    .await
    .map_err(ApiFailure::from_engine)?;
    Ok(Json(resp))
}

/// GET /repos/{id}/file — intentionally does NOT snapshot: file reads must be
/// side-effect free (Task 8's watcher test depends on reads never creating
/// snapshot operations).
async fn file(
    State(st): State<DaemonState>,
    Path(id): Path<String>,
    Query(q): Query<FileQuery>,
) -> Result<Response, ApiFailure> {
    let (_, handle) = st.resolve(&id).await.ok_or_else(|| ApiFailure::unknown_repo(&id))?;
    let bytes = run_engine(move || async move {
        let engine = handle.lock().await;
        engine.read_file(&q.rev, &q.path).await
    })
    .await
    .map_err(ApiFailure::from_engine)?;
    Ok(([(header::CONTENT_TYPE, "application/octet-stream")], bytes).into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conflict_engine_error_maps_to_http_conflict() {
        let failure = ApiFailure::from_engine(
            bg_engine::EngineError::Conflict("path is conflicted: src/lib.rs".into()).into(),
        );
        assert_eq!(failure.status, StatusCode::CONFLICT);
        assert_eq!(failure.error.code, ErrorCode::Conflict);
    }
}
