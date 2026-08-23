use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(transparent)]
pub struct RepoId(pub String);

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RegisterRequest {
    pub path: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RepoInfo {
    pub id: RepoId,
    pub root: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WorkspaceInfo {
    pub name: String,
    pub path: PathBuf,
    pub change_id: String,
    pub commit_id: String,
    pub description: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FileChange {
    pub path: String,
    pub kind: ChangeKind,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ChangeKind {
    Added,
    Modified,
    Removed,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WorkspaceStatus {
    pub info: WorkspaceInfo,
    pub parent_change_id: String,
    pub changed_files: Vec<FileChange>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StatusResponse {
    pub repo: RepoInfo,
    pub workspaces: Vec<WorkspaceStatus>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LogEntry {
    pub change_id: String,
    pub commit_id: String,
    pub description: String,
    pub author_name: String,
    pub author_email: String,
    pub timestamp_ms: u64,
    pub bookmarks: Vec<String>,
    pub is_working_copy: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DescribeRequest {
    pub workspace: Option<String>,
    pub change_id: Option<String>,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct NewWorkspaceRequest {
    pub name: String,
    pub dest: Option<PathBuf>,
    pub at_change: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PushRequest {
    pub change_id: Option<String>,
    pub remote: String,
    pub bookmark: String,
    pub create: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PushResponse {
    pub remote: String,
    pub bookmark: String,
    pub commit_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FileQuery {
    pub rev: String,
    pub path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    NotFound,
    Conflict,
    GuardrailRefused,
    Internal,
    InvalidRequest,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ApiError {
    pub code: ErrorCode,
    pub message: String,
    pub hint: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_status_response() {
        let resp = StatusResponse {
            repo: RepoInfo { id: RepoId("demo".into()), root: "/tmp/demo".into() },
            workspaces: vec![WorkspaceStatus {
                info: WorkspaceInfo {
                    name: "default".into(),
                    path: "/tmp/demo".into(),
                    change_id: "abc123".into(),
                    commit_id: "def456".into(),
                    description: "".into(),
                },
                parent_change_id: "p1".into(),
                changed_files: vec![FileChange { path: "src/main.rs".into(), kind: ChangeKind::Modified }],
            }],
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert_eq!(serde_json::from_str::<StatusResponse>(&json).unwrap(), resp);
    }

    #[test]
    fn error_codes_serialize_snake_case() {
        let e = ApiError { code: ErrorCode::GuardrailRefused, message: "no".into(), hint: None };
        assert!(serde_json::to_string(&e).unwrap().contains("guardrail_refused"));
    }
}
