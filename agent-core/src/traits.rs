use async_trait::async_trait;
use anyhow::Result;
use crate::models::{AgentMessage, ChatMessage, ToolDefinition};

/// Token usage report returned alongside every LLM response.
#[derive(Debug, Clone, Default)]
pub struct TokenUsage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
}

impl TokenUsage {
    pub fn total(&self) -> u64 {
        self.prompt_tokens + self.completion_tokens
    }
}

/// The result of a single LLM generation call.
#[derive(Debug, Clone)]
pub struct LlmResponse {
    pub message: AgentMessage,
    pub usage: TokenUsage,
    /// Raw text before parsing (useful for debugging).
    pub raw_text: String,
}

/// Abstraction over various LLM Providers (OpenAI, Anthropic, Ollama, Candle, etc.)
///
/// Every provider must:
///   1. Accept structured chat messages (not raw strings).
///   2. Accept optional tool definitions for function-calling.
///   3. Return a strictly-parsed `AgentMessage` inside an `LlmResponse`.
///   4. Report token usage for budget tracking.
#[async_trait]
pub trait LlmProvider: Send + Sync {
    /// The human-readable name of this provider (e.g. "openai-gpt4o").
    fn name(&self) -> &str;

    /// Send structured messages + optional tools to the LLM.
    async fn generate(
        &self,
        messages: &[ChatMessage],
        tools: &[ToolDefinition],
        temperature: f32,
    ) -> Result<LlmResponse>;

    /// Maximum context window size (in tokens) for this provider/model.
    fn max_context_window(&self) -> usize;

    /// Count tokens for a given text using this provider's tokenizer.
    fn count_tokens(&self, text: &str) -> usize;
}
