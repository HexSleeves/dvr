use axum::extract::{Path, Query, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use bg_proto::{
    ApiError, DescribeRequest, ErrorCode, FileQuery, LogEntry, RegisterRequest, RepoInfo,
    StatusResponse,
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
        .route("/repos/{id}/file", get(file))
        .with_state(state)
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
            Some(bg_engine::EngineError::Guardrail(_)) => Self::new(
                StatusCode::FORBIDDEN,
                ErrorCode::GuardrailRefused,
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
