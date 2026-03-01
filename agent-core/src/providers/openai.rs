use async_trait::async_trait;
use anyhow::{Result, Context, bail};
use reqwest::{Client, header};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tracing::{debug, warn};
use uuid::Uuid;
use chrono::Utc;
use std::time::Duration;

use crate::models::{AgentAction, AgentMessage, ChatMessage, ToolDefinition};
use crate::traits::{LlmProvider, LlmResponse, TokenUsage};
use crate::protocol::{BrainPayload, BrainResponsePayload};

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

/// Inline struct for fallback step 3: LLM emits `{"reasoning": ..., "action": {...}}`
#[derive(Deserialize)]
struct WrappedAction {
    reasoning: Option<String>,
    action: AgentAction,
}

/// Attempts a 4-step fallback cascade when the primary envelope parse fails.
/// Returns `Some((reasoning, action))` on first successful parse, `None` if all fail.
fn try_fallback_parse(text: &str) -> Option<(Option<String>, AgentAction)> {
    // Step 1: Bare BrainPayload — LLM emitted the payload without the envelope wrapper
    if let Ok(payload) = serde_json::from_str::<BrainPayload>(text) {
        debug!("Fallback step 1: parsed bare BrainPayload");
        return Some((None, AgentAction::BrainActionPlan(payload)));
    }

    // Step 2: Bare AgentAction — LLM emitted a direct action like {"type": "ReturnResult", ...}
    if let Ok(action) = serde_json::from_str::<AgentAction>(text) {
        debug!("Fallback step 2: parsed bare AgentAction");
        return Some((None, action));
    }

    // Step 3: Wrapped AgentAction — LLM emitted {"reasoning": ..., "action": {...}}
    if let Ok(wrapped) = serde_json::from_str::<WrappedAction>(text) {
        debug!("Fallback step 3: parsed WrappedAction (reasoning + action)");
        return Some((wrapped.reasoning, wrapped.action));
    }

    // Step 4: Bare BrainResponsePayload — missing the "type": "response" discriminator
    if let Ok(response) = serde_json::from_str::<BrainResponsePayload>(text) {
        debug!("Fallback step 4: parsed bare BrainResponsePayload, wrapping as BrainPayload::Response");
        return Some((None, AgentAction::BrainActionPlan(BrainPayload::Response(response))));
    }

    None
}

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
                // Try fallback cascade before giving up
                if let Some((fallback_reasoning, fallback_action)) = try_fallback_parse(&clean_text) {
                    warn!("Envelope parse failed, but fallback succeeded. Original error: {}", e);
                    (fallback_reasoning, fallback_action)
                } else {
                    warn!(error = %e, text = %raw_text, "Failed to parse BrainPayload JSON.");
                    (
                        Some(format!("Parse error: {}", e)),
                        AgentAction::AskHuman {
                            question: format!("Failed to parse the brain's JSON response format. Error: {}. Type 'exit' to abort.", e).into(),
                        },
                    )
                }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::traits::LlmProvider;

    // ── OpenAiProvider::new — max_context_window mapping ────────────

    fn make_provider(model: &str) -> OpenAiProvider {
        OpenAiProvider::new(
            "test".into(),
            "sk-test-key".into(),
            "https://api.example.com".into(),
            model.into(),
        )
        .expect("provider construction should not fail")
    }

    #[test]
    fn max_context_gpt4o() {
        let p = make_provider("gpt-4o");
        assert_eq!(p.max_context_window(), 128_000);
    }

    #[test]
    fn max_context_gpt4_turbo() {
        let p = make_provider("gpt-4-turbo");
        assert_eq!(p.max_context_window(), 128_000);
    }

    #[test]
    fn max_context_gpt35() {
        let p = make_provider("gpt-3.5-turbo");
        assert_eq!(p.max_context_window(), 16_385);
    }

    #[test]
    fn max_context_llama() {
        let p = make_provider("llama-3.1-8b");
        assert_eq!(p.max_context_window(), 8_192);
    }

    #[test]
    fn max_context_unknown_defaults_to_128k() {
        let p = make_provider("some-custom-model");
        assert_eq!(p.max_context_window(), 128_000);
    }

    // ── try_fallback_parse — Step 1: Bare BrainPayload ─────────────

    #[test]
    fn fallback_step1_bare_brain_payload() {
        let json = r#"{
            "type": "response",
            "response_type": "answer",
            "confidence": "high",
            "content": "This is the answer.",
            "actions": [],
            "warnings": [],
            "fork": [],
            "references": []
        }"#;

        let result = try_fallback_parse(json);
        assert!(result.is_some(), "Step 1 should parse bare BrainPayload");
        let (reasoning, action) = result.unwrap();
        assert!(reasoning.is_none());
        match action {
            AgentAction::BrainActionPlan(BrainPayload::Response(resp)) => {
                assert_eq!(resp.content, "This is the answer.");
            }
            other => panic!("Expected BrainActionPlan(Response), got {:?}", other),
        }
    }

    #[test]
    fn fallback_step1_bare_brain_payload_clarification() {
        let json = r#"{
            "type": "clarification",
            "needs": [{"what": "hostname", "why": "to connect", "required": true}],
            "partial_analysis": null
        }"#;

        let result = try_fallback_parse(json);
        assert!(result.is_some(), "Step 1 should parse BrainPayload::Clarification");
        let (_, action) = result.unwrap();
        match action {
            AgentAction::BrainActionPlan(BrainPayload::Clarification(c)) => {
                assert_eq!(c.needs.len(), 1);
                assert_eq!(c.needs[0].what, "hostname");
            }
            other => panic!("Expected BrainActionPlan(Clarification), got {:?}", other),
        }
    }

    // ── try_fallback_parse — Step 2: Bare AgentAction ──────────────

    #[test]
    fn fallback_step2_bare_return_result() {
        // No "type": "response" (BrainPayload tag), so step 1 fails.
        // Has "type": "ReturnResult" (AgentAction tag), step 2 catches it.
        let json = r#"{
            "type": "ReturnResult",
            "summary": "All done.",
            "data": null
        }"#;

        let result = try_fallback_parse(json);
        assert!(result.is_some(), "Step 2 should parse bare AgentAction");
        let (reasoning, action) = result.unwrap();
        assert!(reasoning.is_none());
        match action {
            AgentAction::ReturnResult { summary, .. } => {
                assert_eq!(summary, "All done.");
            }
            other => panic!("Expected ReturnResult, got {:?}", other),
        }
    }

    #[test]
    fn fallback_step2_bare_ask_human() {
        let json = r#"{
            "type": "AskHuman",
            "question": "What should I do?"
        }"#;

        let result = try_fallback_parse(json);
        assert!(result.is_some());
        let (_, action) = result.unwrap();
        match action {
            AgentAction::AskHuman { question } => {
                assert_eq!(question, "What should I do?");
            }
            other => panic!("Expected AskHuman, got {:?}", other),
        }
    }

    #[test]
    fn fallback_step2_bare_give_up() {
        let json = r#"{
            "type": "GiveUp",
            "reason": "Cannot proceed without root access."
        }"#;

        let result = try_fallback_parse(json);
        assert!(result.is_some());
        let (_, action) = result.unwrap();
        match action {
            AgentAction::GiveUp { reason, .. } => {
                assert_eq!(reason, "Cannot proceed without root access.");
            }
            other => panic!("Expected GiveUp, got {:?}", other),
        }
    }

    // ── try_fallback_parse — Step 3: WrappedAction ─────────────────

    #[test]
    fn fallback_step3_wrapped_action_with_reasoning() {
        // The exact bug shape from the issue: {"reasoning": ..., "action": {...}}
        let json = r#"{
            "reasoning": "The user asked me to explain the file.",
            "action": {
                "type": "ReturnResult",
                "summary": "Here is the walkthrough.",
                "data": null
            }
        }"#;

        let result = try_fallback_parse(json);
        assert!(result.is_some(), "Step 3 should parse WrappedAction");
        let (reasoning, action) = result.unwrap();
        assert_eq!(reasoning.as_deref(), Some("The user asked me to explain the file."));
        match action {
            AgentAction::ReturnResult { summary, .. } => {
                assert_eq!(summary, "Here is the walkthrough.");
            }
            other => panic!("Expected ReturnResult, got {:?}", other),
        }
    }

    #[test]
    fn fallback_step3_wrapped_action_null_reasoning() {
        // The exact JSON from the bug report
        let json = r#"{
            "reasoning": null,
            "action": {
                "type": "ReturnResult",
                "summary": "File contents explained.",
                "data": null
            }
        }"#;

        let result = try_fallback_parse(json);
        assert!(result.is_some(), "Step 3 should handle null reasoning");
        let (reasoning, action) = result.unwrap();
        assert!(reasoning.is_none());
        match action {
            AgentAction::ReturnResult { summary, .. } => {
                assert_eq!(summary, "File contents explained.");
            }
            other => panic!("Expected ReturnResult, got {:?}", other),
        }
    }

    // ── try_fallback_parse — Step 4: Bare BrainResponsePayload ─────

    #[test]
    fn fallback_step4_bare_brain_response_payload() {
        // No "type" field at all — step 1 fails (needs "type": "response"),
        // step 2 fails (needs "type": "ReturnResult" etc),
        // step 3 fails (needs "action" field),
        // step 4 catches: plain struct with response_type, confidence, content
        let json = r#"{
            "response_type": "answer",
            "confidence": "high",
            "content": "The kernel version is 6.8.",
            "actions": [],
            "warnings": [],
            "fork": [],
            "references": []
        }"#;

        let result = try_fallback_parse(json);
        assert!(result.is_some(), "Step 4 should parse bare BrainResponsePayload");
        let (reasoning, action) = result.unwrap();
        assert!(reasoning.is_none());
        match action {
            AgentAction::BrainActionPlan(BrainPayload::Response(resp)) => {
                assert_eq!(resp.content, "The kernel version is 6.8.");
            }
            other => panic!("Expected BrainActionPlan(Response), got {:?}", other),
        }
    }

    // ── try_fallback_parse — Total failure ─────────────────────────

    #[test]
    fn fallback_all_steps_fail_returns_none() {
        let json = r#"{"foo": "bar", "baz": 42}"#;
        assert!(try_fallback_parse(json).is_none());
    }

    #[test]
    fn fallback_empty_object_returns_none() {
        assert!(try_fallback_parse("{}").is_none());
    }

    #[test]
    fn fallback_invalid_json_returns_none() {
        assert!(try_fallback_parse("not json at all").is_none());
    }

    // ── try_fallback_parse — Step ordering / no shadowing ──────────

    #[test]
    fn step3_not_shadowed_by_step2() {
        // This shape has no top-level "type" field, so step 2 (AgentAction,
        // which is #[serde(tag = "type")]) MUST fail. Step 3 should catch it.
        let json = r#"{
            "reasoning": "thinking",
            "action": {"type": "AskHuman", "question": "Which host?"}
        }"#;

        // Verify step 2 alone would fail
        let step2_attempt = serde_json::from_str::<AgentAction>(json);
        assert!(step2_attempt.is_err(), "Step 2 must not match WrappedAction shape");

        // Verify the cascade routes it to step 3
        let result = try_fallback_parse(json);
        assert!(result.is_some());
        let (reasoning, action) = result.unwrap();
        assert_eq!(reasoning.as_deref(), Some("thinking"));
        match action {
            AgentAction::AskHuman { question } => assert_eq!(question, "Which host?"),
            other => panic!("Expected AskHuman from step 3, got {:?}", other),
        }
    }

    #[test]
    fn step4_not_shadowed_by_step1() {
        // BrainPayload requires "type": "response" discriminator.
        // BrainResponsePayload (plain struct) does NOT have "type".
        // So step 1 must fail, step 4 must catch it.
        let json = r#"{
            "response_type": "analysis",
            "confidence": "medium",
            "content": "Disk usage is at 90%.",
            "actions": [],
            "warnings": ["Disk nearly full"],
            "fork": [],
            "references": []
        }"#;

        // Verify step 1 alone would fail
        let step1_attempt = serde_json::from_str::<BrainPayload>(json);
        assert!(step1_attempt.is_err(), "Step 1 must not match bare BrainResponsePayload");

        let result = try_fallback_parse(json);
        assert!(result.is_some());
        let (_, action) = result.unwrap();
        match action {
            AgentAction::BrainActionPlan(BrainPayload::Response(resp)) => {
                assert_eq!(resp.content, "Disk usage is at 90%.");
                assert_eq!(resp.warnings, vec!["Disk nearly full"]);
            }
            other => panic!("Expected BrainActionPlan(Response) from step 4, got {:?}", other),
        }
    }
}
