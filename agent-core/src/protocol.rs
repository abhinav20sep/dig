use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

/// Base Envelope (ALL messages)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageEnvelope<T> {
    pub proto_version: String, // Expected "1.1"
    pub msg_id: String,
    pub session_id: String,
    pub parent_id: Option<String>,
    pub timestamp: DateTime<Utc>,
    pub direction: Direction,
    pub priority: Priority,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    pub payload: T,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Direction {
    #[serde(rename = "exec→brain")]
    ExecToBrain,
    #[serde(rename = "brain→exec")]
    BrainToExec,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Priority {
    Critical,
    High,
    Normal,
    Low,
}

// ── Executor -> Brain Payloads ─────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
#[serde(rename_all = "snake_case")]
pub enum ExecutorPayload {
    Query(QueryPayload),
    ActionResults(ActionResultsPayload),
    StreamChunk(StreamChunkPayload),
    ContextPush(ContextPushPayload),
    ContextPop(ContextPopPayload),
    SessionControl(SessionControlPayload),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryPayload {
    pub intent: Intent,
    pub context: ExecutorContext,
    pub query: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<Attachment>,
    pub constraints: Constraints,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub history_refs: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Intent {
    Diagnose,
    Explain,
    Plan,
    Lookup,
    AnalyzeOutput,
    Verify,
    ClarifyResponse,
    Freeform,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutorContext {
    pub hostname: String,
    pub distro: String,
    pub kernel: String,
    pub arch: String,
    pub shell: String,
    pub environment: String,
    pub user: String,
    pub network_role: String,
    pub pipe_position: String,
    pub interactive: bool,
    pub tty_available: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Attachment {
    #[serde(rename = "type")]
    pub attachment_type: String,
    pub label: String,
    pub content: String,
    pub truncated: bool,
    pub lines: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Constraints {
    pub read_only: bool,
    pub no_reboot: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub no_service_restart: Vec<String>,
    pub max_downtime: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub approved_scope: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub forbidden: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActionResultsPayload {
    pub results: Vec<BatchResult>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamChunkPayload {
    pub action_id: String,
    pub chunk_seq: u32,
    pub content: String,
    pub status: String,
    pub elapsed_s: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextPushPayload {
    pub reason: String,
    pub trigger_command: String,
    pub new_context: ExecutorContext,
    pub context_depth: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextPopPayload {
    pub trigger_command: String,
    pub returned_to: String,
    pub context_depth: u32,
}

// ── Brain -> Executor Payloads ─────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
#[serde(rename_all = "snake_case")]
pub enum BrainPayload {
    Response(BrainResponsePayload),
    Clarification(BrainClarificationPayload),
    FetchChunk(FetchChunkPayload),
    Signal(SignalPayload),
    SessionControl(SessionControlPayload),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BrainResponsePayload {
    pub response_type: ResponseType,
    pub confidence: Confidence,
    pub content: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub actions: Vec<BrainAction>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fork: Vec<ForkCondition>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub references: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BrainClarificationPayload {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub needs: Vec<Requirement>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub partial_analysis: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FetchChunkPayload {
    pub handle: String,
    pub range: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignalPayload {
    pub action_id: String,
    pub signal: String,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ResponseType {
    Answer,
    ActionPlan,
    ClarificationNeeded,
    Analysis,
    Partial,
    Error,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Confidence {
    High,
    Medium,
    Low,
    Speculative,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BrainAction {
    pub action_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sequence: Option<u32>,
    pub command: String,
    pub purpose: String,
    #[serde(default = "default_shell")]
    pub shell: String,
    #[serde(default = "default_user")]
    pub run_as: String,
    #[serde(default = "default_exec_mode")]
    pub exec_mode: ExecMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_s: Option<u32>,
    #[serde(default = "default_on_timeout")]
    pub on_timeout: OnTimeout,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expect: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_fail: Option<String>,
    #[serde(default)]
    pub destructive: bool,
    #[serde(default)]
    pub send_output_back: bool,
    #[serde(default)]
    pub expect_context_push: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ExecMode {
    Sync,
    Streaming,
    FireForget,
    ContextShift,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OnTimeout {
    KillCollect,
    SignalThenCollect,
    Abandon,
    Ask,
}

fn default_shell() -> String {
    "bash".to_string()
}
fn default_user() -> String {
    "root".to_string()
}
fn default_exec_mode() -> ExecMode {
    ExecMode::Sync
}
fn default_on_timeout() -> OnTimeout {
    OnTimeout::KillCollect
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Requirement {
    pub what: String,
    pub why: String,
    pub suggested_command: Option<String>,
    #[serde(default)]
    pub required: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ForkCondition {
    pub condition: String,
    pub conclusion: String,
    pub next_actions: String,
}

// ── Parallel Execution Batch ───────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatchPayload {
    pub response_type: ResponseType,
    pub execution_mode: String, // e.g. "parallel"
    pub batch_id: String,
    pub actions: Vec<BrainAction>,
    pub join: JoinCondition,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JoinCondition {
    pub wait_for: Vec<String>, // Step IDs to wait for
    pub then: String,
}

// ── Executor Batch Response ────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutorBatchResponse {
    pub intent: Intent,
    pub batch_id: String,
    pub results: Vec<BatchResult>,
    pub query: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChunkData {
    pub range: String,
    pub data: String,
    pub encoding: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "delivery", rename_all = "snake_case")]
pub enum OutputContent {
    Direct {
        stdout: String,
        stderr: String,
    },
    Chunked {
        handle: String,
        total_lines: usize,
        total_bytes: usize,
        chunk: ChunkData,
        has_more: bool,
        stderr: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatchResult {
    pub action_id: String,
    pub exit_code: i32,
    pub content: OutputContent,
    pub duration_ms: u64,
}

// ── Session Control Messages ───────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionControlPayload {
    #[serde(rename = "type")]
    pub payload_type: String, // "session_control"
    pub action: SessionAction,

    #[serde(flatten)]
    pub params: SessionParams,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SessionAction {
    Open,
    Close,
    Fork,
    Merge,
    Pause,
    Resume,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum SessionParams {
    Open { open_params: OpenParams },
    Fork { fork_params: ForkParams },
    Close { close_params: CloseParams },
    None,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenParams {
    pub title: String,
    pub tags: Vec<String>,
    pub priority: Priority,
    pub context: ExecutorContext,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ForkParams {
    pub from_session: String,
    pub reason: String,
    pub new_session_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CloseParams {
    pub resolution: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub actions_taken: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags_final: Vec<String>,
}

pub fn harvest_context(
    pipe_position: String,
    interactive: bool,
    tty_available: bool,
) -> ExecutorContext {
    let hostname = std::fs::read_to_string("/etc/hostname")
        .unwrap_or_else(|_| "localhost".into())
        .trim()
        .to_string();
    let distro = std::fs::read_to_string("/etc/os-release")
        .unwrap_or_default()
        .lines()
        .find(|l| l.starts_with("PRETTY_NAME="))
        .and_then(|l| l.split('=').nth(1))
        .map(|s| s.trim_matches('"').to_string())
        .unwrap_or_else(|| "Linux".into());
    let kernel = std::process::Command::new("uname")
        .arg("-r")
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|_| "unknown".into());
    let arch = std::process::Command::new("uname")
        .arg("-m")
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|_| "unknown".into());
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "bash".into());
    let user = std::env::var("USER").unwrap_or_else(|_| "unknown".into());

    ExecutorContext {
        hostname,
        distro,
        kernel,
        arch,
        shell,
        environment: "terminal".into(),
        user,
        network_role: "endpoint".into(),
        pipe_position,
        interactive,
        tty_available,
        notes: None,
    }
}
