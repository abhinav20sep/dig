use anyhow::{Result, bail};
use crate::models::{AgentAction, ChatMessage, Role};
use crate::traits::{LlmProvider, LlmResponse};
use std::sync::Arc;
use tracing::{info, warn};

/// The Manager Agent synthesizes unstructured outputs from parallel sub-agents
/// using the "Context Stuffing" pattern.
pub struct ManagerAgent {
    provider: Arc<dyn LlmProvider>,
}

impl ManagerAgent {
    pub fn new(provider: Arc<dyn LlmProvider>) -> Self {
        Self { provider }
    }

    /// Performs "Semantic Reduction" — synthesizes multiple agent reports into
    /// a single actionable verdict.
    pub async fn synthesize_reports(&self, reports: Vec<String>) -> Result<String> {
        if reports.is_empty() {
            return Ok("No reports to synthesize.".into());
        }

        info!(report_count = reports.len(), "Manager Agent: synthesizing reports");

        let numbered = reports
            .iter()
            .enumerate()
            .map(|(i, r)| format!("--- Report #{} ---\n{}", i + 1, r))
            .collect::<Vec<_>>()
            .join("\n\n");

        let messages = vec![
            ChatMessage {
                role: Role::System,
                content: concat!(
                    "You are a Lead Analyst. Below are reports from junior agents. ",
                    "Synthesize them into a single actionable verdict. ",
                    "Resolve conflicts if any. ",
                    "Respond with valid JSON matching the AgentMessage schema, ",
                    "using the ReturnResult action with your synthesis in the 'summary' field."
                ).into(),
            },
            ChatMessage {
                role: Role::User,
                content: numbered,
            },
        ];

        let response: LlmResponse = self.provider.generate(&messages, &[], 0.3).await?;

        // Validate that the LLM returned a ReturnResult
        match &response.message.action {
            AgentAction::ReturnResult { summary, .. } => {
                info!(summary_len = summary.len(), "Synthesis complete");
                Ok(summary.clone())
            }
            AgentAction::GiveUp { reason, .. } => {
                warn!(reason, "Manager gave up during synthesis");
                bail!("Manager agent gave up: {}", reason);
            }
            other => {
                warn!(action = ?other, "Manager returned unexpected action, extracting reasoning");
                // Fallback: use reasoning field if available, or serialize the action
                Ok(response
                    .message
                    .reasoning
                    .unwrap_or_else(|| serde_json::to_string(&response.message.action).unwrap_or_default()))
            }
        }
    }
}
