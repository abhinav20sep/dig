use anyhow::{Result, bail};
use chrono::Utc;
use std::sync::Arc;
use tracing::{info, instrument, warn};
use uuid::Uuid;

use crate::governor::{BudgetEnforcer, TokenGovernor};
use crate::memory::HybridMemory;
use crate::models::{AgentAction, ChatMessage, Role, ToolManifestEntry};
use crate::plugin::PluginManager;
use crate::protocol::{ExecutorPayload, MessageEnvelope};
use crate::sandbox::{ProcessSandbox, truncate_output};
use crate::traits::LlmProvider;
use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

#[async_trait]
pub trait ToolPermitter: Send + Sync {
    async fn check_permission(&self, command: &str, args: &[String]) -> Result<bool>;

}

/// A command captured during dry-run mode (not executed).
#[derive(Debug, Clone)]
pub struct DryRunCommand {
    pub command: String,
    pub purpose: String,
    pub run_as: String,
    pub destructive: bool,
}

pub enum AgentLoopResult {
    Completed(String),
    DryRun(Vec<DryRunCommand>),
    NeedsHuman(crate::protocol::BrainClarificationPayload),
}

pub struct AgentLoop {
    provider: Arc<dyn LlmProvider>,
    memory: Arc<HybridMemory>,
    sandbox: Arc<ProcessSandbox>,
    governor: TokenGovernor,
    enforcer: BudgetEnforcer,
    permitter: Arc<dyn ToolPermitter>,
    tools: Vec<ToolManifestEntry>,
    system_prompt: String,

    // Internal state for an ongoing turn
    chat_history: Vec<ChatMessage>,
    current_turn_count: u32,
    pub context_depth: u32,
    pub pipe_position: String,
    pub interactive: bool,
    pub tty_available: bool,

    // CLI flag overrides
    pub dry_run: bool,
    pub no_cache: bool,
    pub no_memory: bool,

    // Plugin system
    plugin_manager: Option<PluginManager>,
    /// The original user query (set per turn for plugin context)
    current_query: String,
}

impl AgentLoop {
    pub fn new(
        provider: Arc<dyn LlmProvider>,
        memory: Arc<HybridMemory>,
        sandbox: Arc<ProcessSandbox>,
        governor: TokenGovernor,
        enforcer: BudgetEnforcer,
        permitter: Arc<dyn ToolPermitter>,
        tools: Vec<ToolManifestEntry>,
        plugin_manager: Option<PluginManager>,
    ) -> Self {
        let mut system_prompt = r#"You are the Brain component of a SysAdmin LLM architecture.
You receive JSON `MessageEnvelope` requests from the user's terminal containing an `ExecutorPayload`.
You MUST reply with a completely valid stringified JSON matching the v1.1 `MessageEnvelope<BrainPayload>` tagged-union schema.
No markdown block ticks wrapping the JSON. No conversational preamble. Just JSON.

The envelope MUST have `proto_version`: "1.1".
Your `payload` MUST contain a `"type"` discriminator ("response", "clarification", "fetch_chunk", "signal").

CRITICAL INSTRUCTION: You are an agentic terminal executor. If the user's query implies performing an action, finding files, or manipulating the system, you MUST execute the bash commands for them using the `actions` array. DO NOT just verbally instruct the user how to do it. Only use `response_type: "answer"` without actions if the user is explicitly just asking a conceptual question.

DISCOVERY-FIRST RULE — SOFTWARE TASKS: When the user's task involves installing, updating, upgrading, removing, or checking a named software tool or package (e.g. "update claude code", "install neovim", "upgrade node"), you MUST:
1. NEVER ask for clarification about files or directories — these are software/product names, NOT file paths.
2. STEP 1 — CHECK IF INSTALLED: Run `which <tool>` to see if it exists. Binary names often differ from package names (e.g. "claude-code" installs binary "claude", "neovim" installs "nvim", "golang" installs "go"), so try plausible variants: `which <tool> || which <short-name>`. If not found, tell the user and offer to install it. STOP HERE if not found.
3. STEP 2 — LEARN THE TOOL: Run `<tool> --help` to read its capabilities. This tells you what subcommands exist (update, self-update, upgrade, etc.) and how the tool works. Do NOT guess — read the help output first.
4. STEP 3 — ACT: Based on what --help told you, run the exact correct command. Use ONLY what the tool itself documented. NEVER shotgun-chain multiple package managers with `||` (e.g. NEVER do `npm update X || pip install X || apt install X`) — this is dangerous because different package managers may install completely different packages with the same name.
Each step is ONE action. Wait for results before proceeding to the next step.

Schema reference (Response):
{
  "proto_version": "1.1",
  "msg_id": "<uuid>",
  "session_id": "<uuid>",
  "parent_id": null,
  "timestamp": "2026-02-21T00:00:00Z",
  "direction": "brain→exec",
  "priority": "normal",
  "payload": {
    "type": "response",
    "response_type": "answer" | "action_plan" | "analysis" | "partial" | "error",
    "confidence": "high" | "medium" | "low" | "speculative",
    "content": "Description of what you are doing or the final answer.",
    "actions": [
      {
        "action_id": "unique-slug-1",
        "command": "bash command here",
        "purpose": "why",
        "shell": "bash",
        "run_as": "<current_user_from_context>",
        "exec_mode": "sync" | "streaming" | "fire_forget" | "context_shift",
        "timeout_s": 30,
        "on_timeout": "kill_collect" | "signal_then_collect" | "abandon" | "ask",
        "destructive": false,
        "send_output_back": true,
        "expect_context_push": false
      }
    ]
  }
}

Schema reference (Clarification — USE SPARINGLY, see rules below):
{
  "proto_version": "1.1",
  "msg_id": "<uuid>",
  "session_id": "<uuid>",
  "parent_id": null,
  "timestamp": "2026-02-21T00:00:00Z",
  "direction": "brain→exec",
  "priority": "normal",
  "payload": {
    "type": "clarification",
    "needs": [
      {"what": "which of the 3 matching services to restart", "why": "restarting the wrong one could cause downtime", "required": true}
    ]
  }
}

Schema reference (FetchChunk for large output pagination):
{
  "proto_version": "1.1",
  "msg_id": "<uuid>",
  "session_id": "<uuid>",
  "parent_id": null,
  "timestamp": "2026-02-21T00:00:00Z",
  "direction": "brain→exec",
  "priority": "normal",
  "payload": {
    "type": "fetch_chunk",
    "handle": "out-7f3a2b",
    "range": "lines:501-1000"
  }
}

- For simple answers without executing commands, use "type": "response" with "response_type": "answer" and empty "actions".
- Use `action_id` (string slug) for action correlation, NOT integers.
- `run_as` MUST match the current user from the context (e.g. "neo"). Only set "root" when the command genuinely requires sudo (e.g. systemctl, apt install, modifying /etc). Using "root" causes sudo which resets environment variables and can break commands that depend on user-level config.
- For potentially hanging commands, use `timeout_s` proactively.
- If handling chunked output results from the Executor, use "type": "fetch_chunk" to request subsequent lines.
- If switching active environments (e.g. docker exec), set `exec_mode": "context_shift" or "expect_context_push": true.

CLARIFICATION — LAST RESORT ONLY: Only use "type": "clarification" when ALL of these are true:
1. The user's intent is genuinely ambiguous (not just unfamiliar to you).
2. Running an exploratory command (which, find, ls, cat, dpkg, etc.) would NOT resolve the ambiguity.
3. Making the wrong assumption could cause irreversible harm (data loss, wrong service restarted, etc.).
If none of those apply, DO NOT clarify — just act. Prefer running a safe exploratory command over asking.
When you DO use clarification, always provide a sensible DEFAULT:
  "needs": [{ "what": "...", "why": "...", "default": "<safe_default>", "default_label": "..." }]
The default is used when the human cannot respond (piped/headless mode).
If interactive: false, you MUST NOT emit clarifications without a DEFAULT.
If pipe_position: "head", your final output is going to another process (emit raw data, not prose).
- When receiving a query with `intent: "clarify_response"`, this is the user's answer to your previous clarification. You MUST process their answer along with the previous context, and immediately formulate and execute the required bash commands. Do not ask for clarification on the same topic again.
If pipe_position: "tail", your input is machine-generated data from stdin."#.to_string();

        // Append plugin capability descriptions to the system prompt
        if let Some(ref pm) = plugin_manager {
            let cap = pm.capability_prompt();
            if !cap.is_empty() {
                system_prompt.push_str(&cap);
                if std::env::var("DIG_DEBUG").is_ok() {
                    eprintln!("[DIG_DEBUG] Plugin capabilities appended to system prompt ({} chars, {} plugins)", cap.len(), pm.enabled_plugins().len());
                }
            }
        } else if std::env::var("DIG_DEBUG").is_ok() {
            eprintln!("[DIG_DEBUG] No plugin manager loaded");
        }

        Self {
            provider,
            memory,
            sandbox,
            governor,
            enforcer,
            permitter,
            tools,
            system_prompt,
            chat_history: Vec::new(),
            current_turn_count: 0,
            context_depth: 1,
            pipe_position: "none".to_string(),
            interactive: true,
            tty_available: true,
            dry_run: false,
            no_cache: false,
            no_memory: false,
            plugin_manager,
            current_query: String::new(),
        }
    }

    pub fn set_io_context(
        &mut self,
        pipe_position: String,
        interactive: bool,
        tty_available: bool,
    ) {
        self.pipe_position = pipe_position;
        self.interactive = interactive;
        self.tty_available = tty_available;
    }

    /// Reset the internal chat history (used for a brand new conversation).
    pub fn reset(&mut self) {
        self.chat_history.clear();
        self.current_turn_count = 0;
    }

    /// Submit a clarification answer and continue the LLM loop.
    /// This injects the user's answer as a simple chat message (not a full protocol envelope)
    /// so the LLM naturally correlates it with its previous clarification question.
    pub async fn submit_clarification_answer(
        &mut self,
        answer: &str,
        cancel_token: &CancellationToken,
    ) -> Result<AgentLoopResult> {
        let dig_debug = std::env::var("DIG_DEBUG").is_ok();
        if dig_debug {
            eprintln!("\n[DIG_DEBUG] ===== CLARIFICATION ANSWER =====");
            eprintln!("[DIG_DEBUG] User answered: {}", answer);
            eprintln!("[DIG_DEBUG] ================================\n");
        }

        self.chat_history.push(ChatMessage {
            role: Role::User,
            content: format!("User's clarification answer: {}", answer),
        });

        self.drive_loop(cancel_token).await
    }

    /// Start or continue an interaction loop.
    #[instrument(skip(self, envelope, cancel_token))]
    pub async fn run_turn(
        &mut self,
        envelope: &MessageEnvelope<ExecutorPayload>,
        cancel_token: &CancellationToken,
    ) -> Result<AgentLoopResult> {
        let user_query = match &envelope.payload {
            crate::protocol::ExecutorPayload::Query(q) => q.query.clone(),
            _ => {
                return Err(anyhow::anyhow!(
                    "Expected ExecutorPayload::Query as entrypoint"
                ));
            }
        };
        self.current_query = user_query.clone();
        let payload_json = serde_json::to_string_pretty(envelope)
            .unwrap_or_else(|_| "Error serializing payload".into());

        if self.chat_history.is_empty() {
            let context_block = if self.no_memory {
                String::new()
            } else {
                self.memory.get_context_block(&user_query).await?
            };
            self.chat_history.push(ChatMessage {
                role: Role::System,
                content: format!("{}\n\n[CONTEXT]\n{}", self.system_prompt, context_block),
            });
            self.chat_history.push(ChatMessage {
                role: Role::User,
                content: payload_json.clone(),
            });
        } else {
            self.chat_history.push(ChatMessage {
                role: Role::User,
                content: payload_json.clone(),
            });
        }

        self.drive_loop(cancel_token).await
    }

    /// Internal LLM loop: calls the LLM, dispatches actions, handles tool execution.
    /// Shared by both `run_turn()` and `submit_clarification_answer()`.
    async fn drive_loop(&mut self, cancel_token: &CancellationToken) -> Result<AgentLoopResult> {
        // Extract the plain text query from the envelope JSON for cache matching
        let user_query = self
            .chat_history
            .get(1)
            .and_then(|m| {
                serde_json::from_str::<serde_json::Value>(&m.content)
                    .ok()
                    .and_then(|v| v["payload"]["query"].as_str().map(|s| s.to_string()))
            })
            .unwrap_or_else(|| {
                self.chat_history
                    .get(1)
                    .map(|m| m.content.clone())
                    .unwrap_or_default()
            });
        let dig_debug = std::env::var("DIG_DEBUG").is_ok();

        loop {
            // 2. Enforce limits
            self.enforcer
                .check(self.enforcer.max_ttl, self.current_turn_count)?;
            if self.governor.is_exhausted() {
                bail!("Token budget exhausted.");
            }

            // 3. Check Command Cache (Jaccard text similarity — no embedding API calls)
            if self.current_turn_count == 0 && !self.no_cache && !self.no_memory {
                if let Ok(Some(cached_response)) =
                    self.memory.find_cached_command(&user_query).await
                {
                    if dig_debug {
                        eprintln!("[DIG_DEBUG] Cache HIT — returning cached response directly");
                    }
                    info!("⚡ Jaccard Cache Hit! Bypassing LLM.");
                    self.reset();
                    return Ok(AgentLoopResult::Completed(cached_response));
                }
            }

            // 4. Call LLM
            let mut msg = {
                if dig_debug {
                    eprintln!("\n[DIG_DEBUG] ================= LLM REQUEST =================");
                    for (i, msg) in self.chat_history.iter().enumerate() {
                        eprintln!(
                            "[DIG_DEBUG] Msg {}: [{:?}] \n{}\n",
                            i, msg.role, msg.content
                        );
                    }
                    eprintln!("[DIG_DEBUG] ================================================\n");
                }
                let response = tokio::select! {
                    _ = cancel_token.cancelled() => {
                        bail!("Turn cancelled by user (SIGINT)");
                    }
                    res = self.provider.generate(&self.chat_history, &[], 0.3) => {
                        res?
                    }
                };
                self.governor.consume(response.usage.total()).await?;

                if dig_debug {
                    eprintln!("\n[DIG_DEBUG] ================= LLM RESPONSE ================");
                    eprintln!(
                        "{}\n[DIG_DEBUG] ================================================\n",
                        response.raw_text
                    );
                }

                self.chat_history.push(ChatMessage {
                    role: Role::Assistant,
                    content: response.raw_text.clone(),
                });

                let mut new_msg = response.message;
                new_msg.agent_id = Uuid::new_v4();
                new_msg.timestamp = Utc::now();
                new_msg
            };

            self.current_turn_count += 1;
            msg.turn_count = self.current_turn_count;

            if let Some(reasoning) = &msg.reasoning {
                info!(turn = self.current_turn_count, reasoning = %reasoning, "Agent thought");
            }

            if dig_debug {
                eprintln!(
                    "[DIG_DEBUG] Action dispatched: {:?}",
                    std::mem::discriminant(&msg.action)
                );
            }

            // 5. Dispatch Action
            match msg.action {
                AgentAction::ReturnResult { data: _, summary } => {
                    info!("Agent reached conclusion.");
                    // Record full turn to memory
                    // We extract the original user input manually (the 2nd message in history)
                    let original_user = self
                        .chat_history
                        .get(1)
                        .map(|m| &m.content)
                        .cloned()
                        .unwrap_or_default();
                    if !self.no_memory {
                        self.memory.record_turn(&original_user, &summary).await?;
                    }
                    self.reset();
                    return Ok(AgentLoopResult::Completed(summary));
                }
                AgentAction::GiveUp { reason, .. } => {
                    warn!(reason, "Agent gave up");
                    self.reset();
                    bail!("Agent gave up: {}", reason);
                }
                AgentAction::AskHuman { question } => {
                    info!(question, "Agent is asking for human input over dialoguer");
                    let payload = crate::protocol::BrainClarificationPayload {
                        needs: vec![crate::protocol::Requirement {
                            what: "Human Input".to_string(),
                            why: "Requested by Agent".to_string(),
                            suggested_command: None,
                            required: true,
                            default: None,
                            default_label: None,
                            prompt: Some(question),
                        }],
                        partial_analysis: None,
                    };
                    return Ok(AgentLoopResult::NeedsHuman(payload));
                }
                AgentAction::RunAndReturn {
                    tool_name,
                    parameters,
                } => {
                    info!(tool = %tool_name, params = %parameters, "Direct RunAndReturn Tool Execution");

                    if self.dry_run {
                        let cmd = format!("{} {}", tool_name, parameters);
                        self.reset();
                        return Ok(AgentLoopResult::DryRun(vec![DryRunCommand {
                            command: cmd,
                            purpose: "RunAndReturn tool execution".into(),
                            run_as: "user".into(),
                            destructive: false,
                        }]));
                    }

                    let tool_def = self.tools.iter().find(|t| t.name == tool_name);
                    let result_str = match tool_def {
                        Some(tool) => {
                            let mut args: Vec<String> = tool.args.clone();
                            if let serde_json::Value::Object(map) = parameters.clone() {
                                for (_, v) in map {
                                    match v {
                                        serde_json::Value::String(s) => args.push(s),
                                        serde_json::Value::Null => {}
                                        other => args.push(other.to_string()),
                                    }
                                }
                            }
                            let permitted = self
                                .permitter
                                .check_permission(&tool.command, &args)
                                .await
                                .unwrap_or(false);
                            if !permitted {
                                format!("Error: Execution denied by user.")
                            } else {
                                let args_refs: Vec<&str> =
                                    args.iter().map(|s| s.as_str()).collect();
                                match self
                                    .sandbox
                                    .execute(
                                        &tool.command,
                                        &args_refs,
                                        None,
                                        &crate::protocol::OnTimeout::KillCollect,
                                    )
                                    .await
                                {
                                    Ok(res) => {
                                        let mut out = String::new();
                                        let truncated_stdout = truncate_output(&res.stdout);
                                        if !truncated_stdout.trim().is_empty() {
                                            out.push_str(truncated_stdout.trim());
                                        }
                                        if !res.stderr.trim().is_empty() {
                                            if !out.is_empty() {
                                                out.push('\n');
                                            }
                                            out.push_str(res.stderr.trim());
                                        }
                                        if out.is_empty() {
                                            out.push_str(
                                                "Command executed successfully with no output.",
                                            );
                                        }
                                        if !res.success {
                                            out.push_str(&format!(
                                                "\nNote: Command exited with failure status ({}).",
                                                res.exit_code
                                            ));
                                        }
                                        let original_user = self
                                            .chat_history
                                            .get(1)
                                            .map(|m| &m.content)
                                            .cloned()
                                            .unwrap_or_default();
                                        if !self.no_memory {
                                            let _ = self.memory.record_turn(&original_user, &out).await;
                                        }
                                        self.reset();
                                        return Ok(AgentLoopResult::Completed(out));
                                    }
                                    Err(e) => format!("Execution Sandbox Error: {}", e),
                                }
                            }
                        }
                        None => format!("Error: Tool '{}' not found in manifest.", tool_name),
                    };

                    self.reset();
                    return Ok(AgentLoopResult::Completed(result_str));
                }
                AgentAction::ExecuteTool {
                    tool_name,
                    parameters,
                } => {
                    info!(tool = %tool_name, params = %parameters, "Executing Tool");

                    if self.dry_run {
                        let cmd = format!("{} {}", tool_name, parameters);
                        self.reset();
                        return Ok(AgentLoopResult::DryRun(vec![DryRunCommand {
                            command: cmd,
                            purpose: "ExecuteTool invocation".into(),
                            run_as: "user".into(),
                            destructive: false,
                        }]));
                    }

                    // Verify tool exists in manifest
                    let tool_def = self.tools.iter().find(|t| t.name == tool_name);
                    let result_str = match tool_def {
                        Some(tool) => {
                            let mut args: Vec<String> = tool.args.clone();
                            // Flatten all JSON parameter values into string args.
                            if let serde_json::Value::Object(map) = parameters.clone() {
                                for (_, v) in map {
                                    match v {
                                        serde_json::Value::String(s) => args.push(s),
                                        serde_json::Value::Null => {}
                                        other => args.push(other.to_string()),
                                    }
                                }
                            }
                            let permitted = self
                                .permitter
                                .check_permission(&tool.command, &args)
                                .await
                                .unwrap_or(false);
                            if !permitted {
                                format!("Error: Execution denied by user.")
                            } else {
                                let args_refs: Vec<&str> =
                                    args.iter().map(|s| s.as_str()).collect();
                                match self
                                    .sandbox
                                    .execute(
                                        &tool.command,
                                        &args_refs,
                                        None,
                                        &crate::protocol::OnTimeout::KillCollect,
                                    )
                                    .await
                                {
                                    Ok(res) => {
                                        let mut out = String::new();
                                        let truncated_stdout = truncate_output(&res.stdout);
                                        let truncated_stderr = truncate_output(&res.stderr);
                                        if !truncated_stdout.trim().is_empty() {
                                            out.push_str(&format!(
                                                "STDOUT:\n{}\n",
                                                truncated_stdout
                                            ));
                                        }
                                        if !truncated_stderr.trim().is_empty() {
                                            out.push_str(&format!(
                                                "STDERR:\n{}\n",
                                                truncated_stderr
                                            ));
                                        }
                                        if out.is_empty() {
                                            out.push_str(
                                                "Command executed successfully with no output.",
                                            );
                                        }
                                        if !res.success {
                                            out.push_str(&format!(
                                                "\nNote: Command exited with failure status ({}).",
                                                res.exit_code
                                            ));
                                        }
                                        out
                                    }
                                    Err(e) => format!("Execution Sandbox Error: {}", e),
                                }
                            }
                        }
                        None => format!("Error: Tool '{}' not found in manifest.", tool_name),
                    };

                    // Add tool result to chat history
                    self.chat_history.push(ChatMessage {
                        role: Role::System,
                        content: format!("Tool Result ({}):\n{}", tool_name, result_str),
                    });
                }
                AgentAction::BrainActionPlan(payload) => {
                    use crate::protocol::BrainPayload;
                    match payload {
                        BrainPayload::Response(resp) => {
                            // Return terminal responses (answer, error, analysis) directly to user
                            if resp.actions.is_empty() {
                                if !self.no_memory {
                                    let _ = self.memory.cache_command(&user_query, &resp.content).await;
                                    self.memory.record_turn(&user_query, &resp.content).await?;
                                }
                                self.reset();
                                return Ok(AgentLoopResult::Completed(resp.content));
                            }

                            // Dry-run: capture commands without executing
                            if self.dry_run {
                                let commands: Vec<DryRunCommand> = resp.actions.iter().map(|a| {
                                    DryRunCommand {
                                        command: a.command.clone(),
                                        purpose: a.purpose.clone(),
                                        run_as: a.run_as.clone(),
                                        destructive: a.destructive,
                                    }
                                }).collect();
                                self.reset();
                                return Ok(AgentLoopResult::DryRun(commands));
                            }

                            let mut cumulative_output = String::new();
                            let mut sent_output_back = false;
                            let mut batch_results = Vec::new();

                            for action in resp.actions {
                                info!(action_id = %action.action_id, command = %action.command, "Executing BrainAction");

                                // Plugin interception: handle dig-plugin commands
                                // Plugins are always permitted (no sandbox cost)
                                if let Some((plugin_name, plugin_args)) =
                                    PluginManager::parse_plugin_command(&action.command)
                                {
                                    if let Some(ref pm) = self.plugin_manager {
                                        let cwd = std::env::current_dir()
                                            .map(|p| p.to_string_lossy().to_string())
                                            .unwrap_or_else(|_| ".".to_string());
                                        let plugin_output = pm
                                            .execute(
                                                &plugin_name,
                                                &plugin_args,
                                                &cwd,
                                                &self.current_query,
                                            )
                                            .await;
                                        let output_text = match plugin_output {
                                            Ok(text) => text,
                                            Err(e) => format!("Plugin error: {}", e),
                                        };
                                        sent_output_back = true;
                                        batch_results.push(crate::protocol::BatchResult {
                                            action_id: action.action_id.clone(),
                                            exit_code: 0,
                                            content: crate::protocol::OutputContent::Direct {
                                                stdout: output_text,
                                                stderr: String::new(),
                                            },
                                            duration_ms: 0,
                                        });
                                        continue;
                                    } else {
                                        sent_output_back = true;
                                        batch_results.push(crate::protocol::BatchResult {
                                            action_id: action.action_id.clone(),
                                            exit_code: 1,
                                            content: crate::protocol::OutputContent::Direct {
                                                stdout: String::new(),
                                                stderr: "No plugins loaded.".to_string(),
                                            },
                                            duration_ms: 0,
                                        });
                                        continue;
                                    }
                                }

                                let permitted = self
                                    .permitter
                                    .check_permission(&action.command, &[])
                                    .await
                                    .unwrap_or(false);
                                if !permitted {
                                    cumulative_output.push_str(&format!(
                                        "Action ID {}: Execution denied.\n",
                                        action.action_id
                                    ));
                                    break;
                                }

                                if action.command.trim() == "exit" && self.context_depth > 1 {
                                    self.context_depth -= 1;
                                    let pop_payload = crate::protocol::ExecutorPayload::ContextPop(
                                        crate::protocol::ContextPopPayload {
                                            trigger_command: action.command.clone(),
                                            returned_to: "parent".to_string(),
                                            context_depth: self.context_depth,
                                        },
                                    );
                                    let env = crate::protocol::MessageEnvelope {
                                        proto_version: "1.1".to_string(),
                                        msg_id: uuid::Uuid::new_v4().to_string(),
                                        session_id: uuid::Uuid::new_v4().to_string(),
                                        parent_id: None,
                                        timestamp: chrono::Utc::now(),
                                        direction: crate::protocol::Direction::ExecToBrain,
                                        priority: crate::protocol::Priority::Normal,
                                        tags: vec![],
                                        payload: pop_payload,
                                    };
                                    cumulative_output.push_str(&format!(
                                        "Action ID {} Context Pop:\n{}\n",
                                        action.action_id,
                                        serde_json::to_string_pretty(&env).unwrap()
                                    ));
                                    continue;
                                }

                                // Respect run_as field: prepend sudo if needed
                                let effective_command = if action.run_as == "root"
                                    && std::env::var("USER").unwrap_or_default() != "root"
                                {
                                    format!("sudo {}", action.command)
                                } else {
                                    action.command.clone()
                                };

                                // Choose streaming vs sync execution
                                let exec_result = if matches!(
                                    action.exec_mode,
                                    crate::protocol::ExecMode::Streaming
                                ) {
                                    // Stream output line-by-line to user's terminal
                                    self.sandbox
                                        .execute_streaming(
                                            "bash",
                                            &["-c", &effective_command],
                                            action.timeout_s,
                                            &action.on_timeout,
                                            |line| {
                                                eprintln!("{}", line);
                                            },
                                        )
                                        .await
                                } else {
                                    self.sandbox
                                        .execute(
                                            "bash",
                                            &["-c", &effective_command],
                                            action.timeout_s,
                                            &action.on_timeout,
                                        )
                                        .await
                                };

                                match exec_result {
                                    Ok(res) => {
                                        if action.expect_context_push
                                            || matches!(
                                                action.exec_mode,
                                                crate::protocol::ExecMode::ContextShift
                                            )
                                        {
                                            self.context_depth += 1;
                                            let push_payload =
                                                crate::protocol::ExecutorPayload::ContextPush(
                                                    crate::protocol::ContextPushPayload {
                                                        reason: action.purpose.clone(),
                                                        trigger_command: action.command.clone(),
                                                        new_context:
                                                            crate::protocol::harvest_context(
                                                                self.pipe_position.clone(),
                                                                self.interactive,
                                                                self.tty_available,
                                                            ),
                                                        context_depth: self.context_depth,
                                                    },
                                                );
                                            let env = crate::protocol::MessageEnvelope {
                                                proto_version: "1.1".to_string(),
                                                msg_id: uuid::Uuid::new_v4().to_string(),
                                                session_id: uuid::Uuid::new_v4().to_string(),
                                                parent_id: None,
                                                timestamp: chrono::Utc::now(),
                                                direction: crate::protocol::Direction::ExecToBrain,
                                                priority: crate::protocol::Priority::Normal,
                                                tags: vec![],
                                                payload: push_payload,
                                            };
                                            cumulative_output.push_str(&format!(
                                                "{}\n",
                                                serde_json::to_string_pretty(&env).unwrap()
                                            ));
                                        }

                                        let content = if res.stdout.len() > 100_000 {
                                            let handle = format!(
                                                "out-{}",
                                                uuid::Uuid::new_v4()
                                                    .as_simple()
                                                    .to_string()
                                                    .chars()
                                                    .take(8)
                                                    .collect::<String>()
                                            );
                                            let tmp_path = format!("/tmp/dig_{}.log", handle);
                                            let _ = std::fs::write(&tmp_path, &res.stdout);
                                            let preview_lines: Vec<&str> =
                                                res.stdout.lines().take(100).collect();
                                            crate::protocol::OutputContent::Chunked {
                                                handle,
                                                total_lines: res.stdout.lines().count(),
                                                total_bytes: res.stdout.len(),
                                                chunk: crate::protocol::ChunkData {
                                                    range: format!(
                                                        "lines:1-{}",
                                                        preview_lines.len()
                                                    ),
                                                    data: preview_lines.join("\n"),
                                                    encoding: "utf-8".to_string(),
                                                },
                                                has_more: true,
                                                stderr: res.stderr.clone(),
                                            }
                                        } else {
                                            crate::protocol::OutputContent::Direct {
                                                stdout: truncate_output(&res.stdout),
                                                stderr: truncate_output(&res.stderr),
                                            }
                                        };

                                        if !action.send_output_back {
                                            // Provide transient feedback if not sent back
                                            cumulative_output.push_str(&format!(
                                                "Action {} executed (silent). Success: {}\n",
                                                action.action_id, res.success
                                            ));
                                        } else {
                                            sent_output_back = true;
                                            batch_results.push(crate::protocol::BatchResult {
                                                action_id: action.action_id.clone(),
                                                exit_code: res.exit_code,
                                                content,
                                                duration_ms: 0,
                                            });
                                        }

                                        if !res.success {
                                            break; // Halt on failure
                                        }

                                    }
                                    Err(e) => {
                                        sent_output_back = true;
                                        batch_results.push(crate::protocol::BatchResult {
                                            action_id: action.action_id.clone(),
                                            exit_code: -1,
                                            content: crate::protocol::OutputContent::Direct {
                                                stdout: String::new(),
                                                stderr: format!("Sandbox Error: {}", e),
                                            },
                                            duration_ms: 0,
                                        });
                                        break;
                                    }
                                }
                            }


                            if !batch_results.is_empty() {
                                let payload = crate::protocol::ExecutorPayload::ActionResults(
                                    crate::protocol::ActionResultsPayload {
                                        results: batch_results,
                                    },
                                );
                                let env = crate::protocol::MessageEnvelope {
                                    proto_version: "1.1".to_string(),
                                    msg_id: uuid::Uuid::new_v4().to_string(),
                                    session_id: uuid::Uuid::new_v4().to_string(),
                                    parent_id: None,
                                    timestamp: chrono::Utc::now(),
                                    direction: crate::protocol::Direction::ExecToBrain,
                                    priority: crate::protocol::Priority::Normal,
                                    tags: vec![],
                                    payload,
                                };
                                cumulative_output.push_str(&format!(
                                    "{}\n",
                                    serde_json::to_string_pretty(&env).unwrap()
                                ));
                            }

                            if !sent_output_back {
                                let original_user = self
                                    .chat_history
                                    .get(1)
                                    .map(|m| &m.content)
                                    .cloned()
                                    .unwrap_or_default();
                                let _ = self
                                    .memory
                                    .record_turn(&original_user, &cumulative_output)
                                    .await;
                                self.reset();
                                return Ok(AgentLoopResult::Completed(cumulative_output));
                            } else {
                                self.chat_history.push(ChatMessage {
                                    role: Role::System,
                                    content: cumulative_output,
                                });
                            }
                        }
                        BrainPayload::FetchChunk(fetch) => {
                            let tmp_path = format!("/tmp/dig_{}.log", fetch.handle);
                            if let Ok(content) = std::fs::read_to_string(&tmp_path) {
                                let lines: Vec<&str> = content.lines().collect();
                                let mut start = 0;
                                let mut end = 100;

                                if fetch.range.starts_with("lines:") {
                                    let parts: Vec<&str> =
                                        fetch.range["lines:".len()..].split('-').collect();
                                    if parts.len() == 2 {
                                        start = parts[0]
                                            .parse::<usize>()
                                            .unwrap_or(1)
                                            .saturating_sub(1);
                                        end = parts[1].parse::<usize>().unwrap_or(100);
                                    }
                                }

                                let chunk_lines: Vec<&str> =
                                    lines.into_iter().skip(start).take(end - start).collect();
                                let chunk_data = chunk_lines.join("\n");

                                let payload = crate::protocol::ExecutorPayload::ActionResults(
                                    crate::protocol::ActionResultsPayload {
                                        results: vec![crate::protocol::BatchResult {
                                            action_id: format!("fetch-{}", fetch.handle),
                                            exit_code: 0,
                                            content: crate::protocol::OutputContent::Chunked {
                                                handle: fetch.handle.clone(),
                                                total_lines: content.lines().count(),
                                                total_bytes: content.len(),
                                                chunk: crate::protocol::ChunkData {
                                                    range: fetch.range.clone(),
                                                    data: chunk_data,
                                                    encoding: "utf-8".to_string(),
                                                },
                                                has_more: end < content.lines().count(),
                                                stderr: String::new(),
                                            },
                                            duration_ms: 0,
                                        }],
                                    },
                                );

                                let env = crate::protocol::MessageEnvelope {
                                    proto_version: "1.1".to_string(),
                                    msg_id: uuid::Uuid::new_v4().to_string(),
                                    session_id: uuid::Uuid::new_v4().to_string(),
                                    parent_id: None,
                                    timestamp: chrono::Utc::now(),
                                    direction: crate::protocol::Direction::ExecToBrain,
                                    priority: crate::protocol::Priority::Normal,
                                    tags: vec![],
                                    payload,
                                };

                                self.chat_history.push(ChatMessage {
                                    role: Role::System,
                                    content: format!(
                                        "{}\n",
                                        serde_json::to_string_pretty(&env).unwrap()
                                    ),
                                });
                            } else {
                                self.chat_history.push(ChatMessage {
                                    role: Role::System,
                                    content: format!(
                                        "Error: Handle {} not found or expired.",
                                        fetch.handle
                                    ),
                                });
                            }
                            continue;
                        }
                        BrainPayload::Clarification(clar) => {
                            return Ok(AgentLoopResult::NeedsHuman(clar));
                        }
                        _ => {
                            self.chat_history.push(ChatMessage {
                                role: Role::System,
                                content: format!("Received unsupported BrainPayload variant."),
                            });
                            continue;
                        }
                    }
                }
            }
        }
    }
}
