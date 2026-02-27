use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

// ── Core Data Types ──────────────────────────────────────────────

/// A single message in a chat history, used by `LlmProvider`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: Role,
    pub content: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

/// A tool definition that can be injected into an LLM prompt for function-calling.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    /// JSON Schema describing the tool's expected parameters.
    pub parameters_schema: Value,
}

// ── Agent Message (the IR) ───────────────────────────────────────

/// The primary IO wrapper for all Agent messages.
/// This struct is the **source of truth** for the JSON schema —
/// the LLM is forced to emit JSON that deserializes into this.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentMessage {
    /// Unique identifier for this agent instance.
    pub agent_id: Uuid,
    /// Who spawned this agent (None for the root agent).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_agent_id: Option<Uuid>,
    /// Time-To-Live counter — decremented on every spawn to prevent infinite recursion.
    pub ttl: u32,
    /// Current reasoning step count within this agent's lifecycle.
    pub turn_count: u32,
    /// Timestamp of when this message was created.
    pub timestamp: DateTime<Utc>,
    /// Agent's internal chain-of-thought (optional, for debugging / logging).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<String>,
    /// The action the agent has decided to take.
    pub action: AgentAction,
}

impl AgentMessage {
    /// Create a new root-level AgentMessage with default safety values.
    pub fn new_root(ttl: u32, action: AgentAction) -> Self {
        Self {
            agent_id: Uuid::new_v4(),
            parent_agent_id: None,
            ttl,
            turn_count: 0,
            timestamp: Utc::now(),
            reasoning: None,
            action,
        }
    }
}

/// The specific actions an agent is allowed to take.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum AgentAction {
    /// Invoke an external tool (bash command, API call, etc.)
    ExecuteTool {
        tool_name: String,
        parameters: Value,
    },
    /// Run a command and immediately return its raw STDOUT/STDERR to the user bypass the LLM.
    RunAndReturn {
        tool_name: String,
        parameters: Value,
    },
    /// Return a structured result back to the parent / orchestrator.
    ReturnResult {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        data: Option<Value>,
        summary: String,
    },
    /// Pause execution and ask the human operator a question.
    AskHuman { question: String },
    /// The agent has exhausted its budget or is stuck — graceful surrender.
    GiveUp {
        reason: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        partial_data: Option<Value>,
    },
    /// A structured Sysadmin Brain Payload.
    BrainActionPlan(crate::protocol::BrainPayload),
}

// ── Configuration Types ──────────────────────────────────────────

/// Top-level application configuration, loaded from `config.toml`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    pub default_model: String,
    pub max_ttl: u32,
    pub max_turns_per_agent: u32,
    pub token_budget: u64,
    pub tokens_per_minute: u32,
    pub max_concurrent_processes: usize,
    pub global_timeout_secs: u64,
    #[serde(default)]
    pub providers: Vec<ProviderConfig>,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            default_model: "gpt-4o".into(),
            max_ttl: 5,
            max_turns_per_agent: 10,
            token_budget: 500_000,
            tokens_per_minute: 90_000,
            max_concurrent_processes: 50,
            global_timeout_secs: 120,
            providers: vec![],
        }
    }
}

/// Configuration for a single LLM provider.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderConfig {
    pub name: String,
    pub api_base: String,
    #[serde(default)]
    pub api_key_env: Option<String>,
    pub model: String,
}

// ── Tool Manifest Types ──────────────────────────────────────────

/// A single tool entry from `tools.toml`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolManifestEntry {
    pub name: String,
    pub description: String,
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    /// JSON Schema of the tool's stdin input.
    #[serde(default)]
    pub input_schema: Option<Value>,
    /// JSON Schema of the tool's stdout output.
    #[serde(default)]
    pub output_schema: Option<Value>,
    /// Security tier: "safe", "confirm", "sandbox"
    #[serde(default = "default_tier")]
    pub tier: String,
}

fn default_tier() -> String {
    "confirm".into()
}
