use async_trait::async_trait;
use anyhow::{Result, Context, bail};
use reqwest::{Client, header};
use serde::Serialize;
use serde_json::Value;
use tracing::{debug, warn};
use uuid::Uuid;
use chrono::Utc;
use std::time::Duration;

use crate::models::{AgentAction, AgentMessage, ChatMessage, ToolDefinition};
use crate::traits::{LlmProvider, LlmResponse, TokenUsage};
use crate::protocol::BrainPayload;

pub struct OpenAiProvider {
    name: String,
    client: Client,
    api_base: String,
    model: String,
    max_context: usize,
}

#[derive(Serialize)]
struct OpenAiRequest<'a> {
    model: &'a str,
    messages: &'a [ChatMessage],
    temperature: f32,
    max_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    response_format: Option<ResponseFormat>,
}

#[derive(Serialize)]
struct ResponseFormat {
    #[serde(rename = "type")]
    format_type: &'static str,
}

/// Compact JSON format from the LLM:
/// {"r":"reason","a":"exec","c":"date"}      → execute bash command
/// {"r":"reason","a":"done","s":"output"}     → return result
/// {"r":"reason","a":"ask","q":"question?"}   → ask user
/// {"r":"reason","a":"give_up","s":"reason"}  → give up
 // Legacy format fallback removed

impl OpenAiProvider {
    pub fn new(name: String, api_key: String, api_base: String, model: String) -> Result<Self> {
        let mut headers = header::HeaderMap::new();
        let mut auth_val = header::HeaderValue::from_str(&format!("Bearer {}", api_key))
            .context("Invalid characters in API key")?;
        auth_val.set_sensitive(true);
        headers.insert(header::AUTHORIZATION, auth_val);

        let client = Client::builder()
            .default_headers(headers)
            .build()?;

        // Example max context mapping based on common models
        let max_context = if model.contains("gpt-4o") || model.contains("gpt-4-turbo") {
            128_000
        } else if model.contains("gpt-3.5") {
            16_385
        } else if model.contains("llama") {
            8_192
        } else {
            128_000 // default fallback
        };

        Ok(Self {
            name,
            client,
            api_base,
            model,
            max_context,
        })
    }
}

#[async_trait]
impl LlmProvider for OpenAiProvider {
    fn name(&self) -> &str {
        &self.name
    }

    async fn generate(
        &self,
        messages: &[ChatMessage],
        _tools: &[ToolDefinition],
        temperature: f32,
    ) -> Result<LlmResponse> {
        // Enforce JSON mode only for models known to support it
        let response_format = if self.model.contains("gpt-") {
            Some(ResponseFormat { format_type: "json_object" })
        } else {
            None
        };

        let req_body = OpenAiRequest {
            model: &self.model,
            messages,
            temperature,
            max_tokens: 4096,
            response_format,
        };

        // If tools are provided, we should ideally inject them into the system prompt 
        // because we are using JSON mode for the AgentAction schema instead of native tool calls.
        // For simplicity now, we assume the system prompt already includes tool context.

        let url = format!("{}/chat/completions", self.api_base);

        // Retry loop with exponential backoff for transient errors
        let max_retries = 3u32;
        let mut attempts = 0u32;
        let resp = loop {
            let response = self.client.post(&url).json(&req_body).send().await?;
            if response.status().is_success() {
                break response;
            }
            let status = response.status().as_u16();
            if (status == 429 || status >= 500) && attempts < max_retries {
                attempts += 1;
                let backoff = Duration::from_millis(500 * 2u64.pow(attempts - 1));
                warn!(status, attempt = attempts, backoff_ms = backoff.as_millis() as u64, "Transient API error — retrying");
                tokio::time::sleep(backoff).await;
                continue;
            }
            let error_text = response.text().await.unwrap_or_default();
            bail!("OpenAI API error ({}): {}", status, error_text);
        };

        let resp_json: Value = resp.json().await?;
        debug!(raw_api_response = ?resp_json, "Raw OpenAI Response Payload");
        
        let raw_text = resp_json["choices"][0]["message"]["content"]
            .as_str()
            .unwrap_or_default()
            .to_string();

        debug!(response_text = %raw_text, "Received LLM response");

        // Parse the strictly typed BrainPayload JSON envelope
        let clean_text = crate::sanitizer::sanitize_response(&self.model, &raw_text);
        let (reasoning, action) = match serde_json::from_str::<crate::protocol::MessageEnvelope<BrainPayload>>(&clean_text) {
            Ok(envelope) => {
                debug!("Successfully parsed MessageEnvelope<BrainPayload>");
                (None, AgentAction::BrainActionPlan(envelope.payload))
            }
            Err(e) => {
                warn!(error = %e, text = %raw_text, "Failed to parse BrainPayload JSON.");
                (
                    Some(format!("Parse error: {}", e)),
                    AgentAction::AskHuman {
                        question: format!("Failed to parse the brain's JSON response format. Error: {}. Type 'exit' to abort.", e).into(),
                    },
                )
            }
        };

        // Construct a generic AgentMessage. The caller (AgentLoop or Orchestrator)
        // is responsible for mapping the correct lineage (agent_id, parent_id, etc.).
        let message = AgentMessage {
            agent_id: Uuid::nil(),
            parent_agent_id: None,
            ttl: 0,
            turn_count: 0,
            timestamp: Utc::now(),
            reasoning,
            action,
        };

        let prompt_tokens = resp_json["usage"]["prompt_tokens"].as_u64().unwrap_or(0);
        let completion_tokens = resp_json["usage"]["completion_tokens"].as_u64().unwrap_or(0);

        Ok(LlmResponse {
            message,
            usage: TokenUsage {
                prompt_tokens,
                completion_tokens,
            },
            raw_text,
        })
    }

    fn max_context_window(&self) -> usize {
        self.max_context
    }

    fn count_tokens(&self, _text: &str) -> usize {
        // In real implementations, this delegates to tiktoken.
        // We rely on the global TokenTracker in `governor.rs` for now.
        0 
    }
}
