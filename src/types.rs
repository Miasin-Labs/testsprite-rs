//! Shared data types. Mirrors `common/interface.ts` + the request/response
//! shapes used by the backend client and tools.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TestType {
    Frontend,
    Backend,
}

impl std::fmt::Display for TestType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TestType::Frontend => write!(f, "frontend"),
            TestType::Backend => write!(f, "backend"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TargetScope {
    Codebase,
    Diff,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ServerMode {
    #[default]
    Development,
    Production,
}

/// `testsprite_tests/tmp/config.json`. Extra fields are preserved on round-trip.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub r#type: Option<TestType>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope: Option<TargetScope>,
    #[serde(rename = "localEndpoint", skip_serializing_if = "Option::is_none")]
    pub local_endpoint: Option<String>,
    #[serde(rename = "projectName", skip_serializing_if = "Option::is_none")]
    pub project_name: Option<String>,
    #[serde(rename = "serverPort", skip_serializing_if = "Option::is_none")]
    pub server_port: Option<u16>,
    #[serde(rename = "serverMode", skip_serializing_if = "Option::is_none")]
    pub server_mode: Option<ServerMode>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub proxy: Option<String>,

    // Backend auth (mirrors the original config fields).
    #[serde(rename = "backendAuthType", skip_serializing_if = "Option::is_none")]
    pub backend_auth_type: Option<String>,
    #[serde(rename = "backendUsername", skip_serializing_if = "Option::is_none")]
    pub backend_username: Option<String>,
    #[serde(rename = "backendPassword", skip_serializing_if = "Option::is_none")]
    pub backend_password: Option<String>,
    #[serde(rename = "backendCredential", skip_serializing_if = "Option::is_none")]
    pub backend_credential: Option<String>,
    #[serde(rename = "backendApiKey", skip_serializing_if = "Option::is_none")]
    pub backend_api_key: Option<String>,
    #[serde(rename = "backendApiValue", skip_serializing_if = "Option::is_none")]
    pub backend_api_value: Option<String>,

    // Frontend login creds.
    #[serde(rename = "loginUser", skip_serializing_if = "Option::is_none")]
    pub login_user: Option<String>,
    #[serde(rename = "loginPassword", skip_serializing_if = "Option::is_none")]
    pub login_password: Option<String>,

    /// Execution args persisted for the `generate-code-and-execute` CLI path.
    #[serde(rename = "executionArgs", skip_serializing_if = "Option::is_none")]
    pub execution_args: Option<ExecutionArgs>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionArgs {
    #[serde(rename = "projectName")]
    pub project_name: String,
    #[serde(rename = "projectPath")]
    pub project_path: String,
    #[serde(rename = "testIds", default)]
    pub test_ids: Vec<String>,
    #[serde(rename = "additionalInstruction", default)]
    pub additional_instruction: String,
    #[serde(rename = "serverMode", default)]
    pub server_mode: ServerMode,
}

/// Account profile from `GET /api/me`.
#[derive(Debug, Clone, Deserialize)]
pub struct AccountInfo {
    #[serde(rename = "firstName")]
    pub first_name: Option<String>,
    #[serde(rename = "lastName")]
    pub last_name: Option<String>,
    pub user: Option<String>,
    #[serde(rename = "subPlan")]
    pub sub_plan: Option<String>,
    pub credits: Option<i64>,
    #[serde(rename = "totalTests")]
    pub total_tests: Option<serde_json::Value>,
}

/// A single planned test case `{id, title, description}`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestCase {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority: Option<String>,
}

/// A test entity returned by the run/poll endpoints.
#[derive(Debug, Clone, Deserialize)]
pub struct TestEntity {
    #[serde(rename = "projectId")]
    pub project_id: Option<String>,
    #[serde(rename = "testId")]
    pub test_id: Option<String>,
    #[serde(rename = "userId")]
    pub user_id: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub code: Option<String>,
    #[serde(rename = "testStatus")]
    pub test_status: Option<String>,
    #[serde(rename = "testError", default)]
    pub test_error: Option<String>,
    #[serde(rename = "testVisualization", default)]
    #[allow(dead_code)]
    pub test_visualization: Option<serde_json::Value>,
    #[serde(default)]
    pub modified: Option<String>,
}

impl TestEntity {
    pub fn is_running(&self) -> bool {
        self.test_status.as_deref() == Some("RUNNING")
    }
    pub fn passed(&self) -> bool {
        self.test_status.as_deref() == Some("PASSED")
    }
}
