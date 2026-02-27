use anyhow::{Result, bail};
use lancedb::query::{ExecutableQuery, QueryBase};
use lancedb::{Connection, connection::ConnectBuilder};
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::governor::TokenCounter;
use crate::models::{AgentAction, ChatMessage, Role};
use crate::providers::embeddings::EmbeddingProvider;
use crate::traits::LlmProvider;

use arrow_array::builder::FixedSizeListBuilder;
use arrow_array::builder::Float32Builder;
use arrow_array::{
    Array, ArrayRef, Int32Array, Int64Array, RecordBatch, RecordBatchIterator, StringArray,
};
use arrow_schema::{DataType, Field, Schema};
use futures::TryStreamExt;
use std::collections::HashSet;

// ── Token Jaccard Helpers ────────────────────────────────────────

/// Tokenize a query into a set of lowercase words, stripping punctuation and stopwords.
fn tokenize(text: &str) -> HashSet<String> {
    const STOPWORDS: &[&str] = &[
        "a", "an", "the", "is", "are", "was", "were", "in", "on", "at", "to", "for", "of", "with",
        "from", "by", "and", "or", "not", "do", "does", "did", "it", "this", "that", "i", "me",
        "my", "we", "our", "you", "your",
    ];
    text.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty() && w.len() > 1 && !STOPWORDS.contains(w))
        .map(|w| w.to_string())
        .collect()
}

/// Jaccard similarity: |A ∩ B| / |A ∪ B|. Returns 0.0 if both sets are empty.
fn jaccard_similarity(a: &HashSet<String>, b: &HashSet<String>) -> f64 {
    if a.is_empty() && b.is_empty() {
        return 0.0;
    }
    let intersection = a.intersection(b).count() as f64;
    let union = a.union(b).count() as f64;
    intersection / union
}

// ── Hybrid Memory ────────────────────────────────────────────────

/// Combines Rolling Summaries (short-term) with LanceDB semantic search (long-term).
#[derive(Clone)]
pub struct HybridMemory {
    db: Arc<Connection>,
    rolling_summary: Arc<RwLock<String>>,
    /// Full turn history (newest last), before compression.
    turn_buffer: Arc<RwLock<Vec<TurnEntry>>>,
    token_counter: Arc<TokenCounter>,
    /// Trigger compression when the turn buffer exceeds this many tokens.
    context_budget: usize,
    /// Embedding provider for semantic search.
    embedder: Arc<dyn EmbeddingProvider>,
    /// LLM provider for summarization during compression.
    summarizer: Arc<dyn LlmProvider>,
    /// Name of the LanceDB table for conversational turns
    table_name: String,
    /// Name of the LanceDB table for command caching
    cache_table_name: String,
    /// Running turn index.
    turn_index: Arc<RwLock<i32>>,
}

/// A single turn in the conversation history.
#[derive(Debug, Clone)]
pub struct TurnEntry {
    pub user_input: String,
    pub agent_output: String,
    pub token_count: usize,
}

impl HybridMemory {
    pub async fn new(
        db_uri: &str,
        token_counter: Arc<TokenCounter>,
        context_budget: usize,
        embedder: Arc<dyn EmbeddingProvider>,
        summarizer: Arc<dyn LlmProvider>,
    ) -> Result<Self> {
        let db = ConnectBuilder::new(db_uri).execute().await?;

        let mem = Self {
            db: Arc::new(db),
            rolling_summary: Arc::new(RwLock::new(String::new())),
            turn_buffer: Arc::new(RwLock::new(Vec::new())),
            token_counter,
            context_budget,
            embedder,
            summarizer,
            table_name: "conversation_memory".to_string(),
            cache_table_name: "command_cache".to_string(),
            turn_index: Arc::new(RwLock::new(0)),
        };

        Ok(mem)
    }

    fn build_schema(&self) -> Arc<Schema> {
        let dim = self.embedder.dimension() as i32;
        Arc::new(Schema::new(vec![
            Field::new("id", DataType::Utf8, false),
            Field::new("text", DataType::Utf8, false),
            Field::new(
                "embedding",
                DataType::FixedSizeList(Arc::new(Field::new("item", DataType::Float32, true)), dim),
                false,
            ),
            Field::new("timestamp", DataType::Int64, false),
            Field::new("turn_index", DataType::Int32, false),
        ]))
    }

    fn build_cache_schema(&self) -> Arc<Schema> {
        Arc::new(Schema::new(vec![
            Field::new("id", DataType::Utf8, false),
            Field::new("query", DataType::Utf8, false),
            Field::new("cmd", DataType::Utf8, false),
            Field::new("timestamp", DataType::Int64, false),
        ]))
    }

    /// Cache a successful text-to-command translation
    pub async fn cache_command(&self, query: &str, bash_cmd: &str) -> Result<()> {
        let schema = self.build_cache_schema();

        let id_array = Arc::new(StringArray::from(vec![Uuid::new_v4().to_string()])) as ArrayRef;
        let query_array = Arc::new(StringArray::from(vec![query.to_string()])) as ArrayRef;
        let cmd_array = Arc::new(StringArray::from(vec![bash_cmd.to_string()])) as ArrayRef;
        let ts = chrono::Utc::now().timestamp();
        let ts_array = Arc::new(Int64Array::from(vec![ts])) as ArrayRef;

        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![id_array, query_array, cmd_array, ts_array],
        )?;

        let batches = RecordBatchIterator::new(vec![Ok(batch)], schema);

        match self.db.open_table(&self.cache_table_name).execute().await {
            Ok(table) => {
                table.add(batches).execute().await?;
            }
            Err(_) => {
                self.db
                    .create_table(&self.cache_table_name, batches)
                    .execute()
                    .await?;
            }
        }

        debug!(query, bash_cmd, "Command cached successfully");
        Ok(())
    }

    /// Find a cached command using token-level Jaccard similarity.
    /// No embedding API calls — purely text-based matching.
    pub async fn find_cached_command(&self, query: &str) -> Result<Option<String>> {
        let table = match self.db.open_table(&self.cache_table_name).execute().await {
            Ok(t) => t,
            Err(_) => {
                let dig_debug = std::env::var("DIG_DEBUG").is_ok();
                if dig_debug {
                    eprintln!("[DIG_DEBUG] Jaccard cache: no cache table yet (empty cache)");
                }
                return Ok(None); // no cache yet
            }
        };

        let results: Vec<RecordBatch> = table
            .query()
            .limit(100) // scan last 100 cached entries
            .execute()
            .await?
            .try_collect()
            .await?;

        let dig_debug = std::env::var("DIG_DEBUG").is_ok();
        let query_tokens = tokenize(query);
        let mut best_score: f64 = 0.0;
        let mut best_cmd: Option<String> = None;
        let mut best_query: String = String::new();

        for batch in &results {
            if let (Some(query_col), Some(cmd_col)) =
                (batch.column_by_name("query"), batch.column_by_name("cmd"))
            {
                if let (Some(query_arr), Some(cmd_arr)) = (
                    query_col.as_any().downcast_ref::<StringArray>(),
                    cmd_col.as_any().downcast_ref::<StringArray>(),
                ) {
                    for i in 0..query_arr.len() {
                        let cached_query = query_arr.value(i);
                        let cached_tokens = tokenize(cached_query);
                        let score = jaccard_similarity(&query_tokens, &cached_tokens);
                        if score > best_score {
                            best_score = score;
                            best_cmd = Some(cmd_arr.value(i).to_string());
                            best_query = cached_query.to_string();
                        }
                    }
                }
            }
        }

        const JACCARD_THRESHOLD: f64 = 0.75;
        if best_score >= JACCARD_THRESHOLD {
            let cmd = best_cmd.unwrap();
            if dig_debug {
                eprintln!(
                    "[DIG_DEBUG] Jaccard cache HIT (score={:.2}, q=\"{}\"): {}",
                    best_score, best_query, cmd
                );
            }
            debug!(score = best_score, cmd = %cmd, "Jaccard cache hit");
            Ok(Some(cmd))
        } else {
            if dig_debug {
                eprintln!(
                    "[DIG_DEBUG] Jaccard cache MISS (best={:.2}, threshold=0.75)",
                    best_score
                );
            }
            debug!(best_score, "Jaccard cache miss");
            Ok(None)
        }
    }

    /// Record a new turn, store embedding, and trigger compression if needed.
    pub async fn record_turn(&self, user_input: &str, agent_output: &str) -> Result<()> {
        let combined = format!("{}\n{}", user_input, agent_output);
        let token_count = self.token_counter.count(&combined);

        let entry = TurnEntry {
            user_input: user_input.to_string(),
            agent_output: agent_output.to_string(),
            token_count,
        };

        // Store embedding in LanceDB (best-effort; don't fail the whole turn)
        if let Err(e) = self.store_embedding(&combined).await {
            warn!(error = %e, "Failed to store embedding — continuing without semantic indexing");
        }

        let mut buffer = self.turn_buffer.write().await;
        buffer.push(entry);

        // ── Check if we need to compress ──
        let total_tokens: usize = buffer.iter().map(|t| t.token_count).sum();
        let threshold = (self.context_budget as f64 * 0.8) as usize;

        if total_tokens > threshold {
            info!(
                total_tokens,
                threshold, "Context 80% full — triggering compression"
            );
            self.compress_oldest(&mut buffer).await?;
        }

        Ok(())
    }

    /// Embed text and store in LanceDB.
    async fn store_embedding(&self, text: &str) -> Result<()> {
        let vecs = self.embedder.embed(&[text.to_string()]).await?;
        if vecs.is_empty() || vecs[0].is_empty() {
            bail!("Empty embedding returned");
        }
        let embedding = &vecs[0];

        let mut idx = self.turn_index.write().await;
        let current_idx = *idx;
        *idx += 1;
        drop(idx);

        let schema = self.build_schema();
        let dim = self.embedder.dimension();

        let id_array = Arc::new(StringArray::from(vec![Uuid::new_v4().to_string()])) as ArrayRef;
        let text_array = Arc::new(StringArray::from(vec![text.to_string()])) as ArrayRef;

        // Build the FixedSizeListArray manually via a builder
        let mut list_builder = FixedSizeListBuilder::new(Float32Builder::new(), dim as i32);
        let values_builder = list_builder.values();
        for &val in embedding {
            values_builder.append_value(val);
        }
        list_builder.append(true);
        let embedding_array = Arc::new(list_builder.finish()) as ArrayRef;

        let ts = chrono::Utc::now().timestamp();
        let ts_array = Arc::new(Int64Array::from(vec![ts])) as ArrayRef;
        let idx_array = Arc::new(Int32Array::from(vec![current_idx])) as ArrayRef;

        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![id_array, text_array, embedding_array, ts_array, idx_array],
        )?;

        let batches = RecordBatchIterator::new(vec![Ok(batch)], schema);

        // Try to open existing table, or create a new one
        match self.db.open_table(&self.table_name).execute().await {
            Ok(table) => {
                table.add(batches).execute().await?;
            }
            Err(_) => {
                // Table doesn't exist yet — create it
                self.db
                    .create_table(&self.table_name, batches)
                    .execute()
                    .await?;
                debug!("Created LanceDB table '{}'", self.table_name);
            }
        }

        debug!(turn_index = current_idx, "Embedding stored");
        Ok(())
    }

    /// Search for similar past turns using vector similarity.
    async fn search_similar(&self, query: &str, top_k: usize) -> Result<Vec<String>> {
        let vecs = self.embedder.embed(&[query.to_string()]).await?;
        if vecs.is_empty() {
            return Ok(vec![]);
        }
        let query_vec = &vecs[0];

        let table = match self.db.open_table(&self.table_name).execute().await {
            Ok(t) => t,
            Err(_) => return Ok(vec![]), // table not created yet — no history
        };

        let results: Vec<RecordBatch> = table
            .vector_search(query_vec.clone())
            .map_err(|e| anyhow::anyhow!("Vector search failed: {}", e))?
            .limit(top_k)
            .execute()
            .await?
            .try_collect()
            .await?;

        let mut hits = Vec::new();
        for batch in &results {
            if let Some(text_col) = batch.column_by_name("text") {
                if let Some(arr) = text_col.as_any().downcast_ref::<StringArray>() {
                    for i in 0..arr.len() {
                        if !arr.is_null(i) {
                            hits.push(arr.value(i).to_string());
                        }
                    }
                }
            }
        }

        debug!(hits = hits.len(), "Semantic search returned results");
        Ok(hits)
    }

    /// Compress the oldest half of the turn buffer using LLM summarization.
    async fn compress_oldest(&self, buffer: &mut Vec<TurnEntry>) -> Result<()> {
        if buffer.is_empty() {
            return Ok(());
        }

        let mid = buffer.len() / 2;
        let to_compress: Vec<TurnEntry> = buffer.drain(..mid).collect();

        // Build a text block from the old turns
        let text: String = to_compress
            .iter()
            .map(|t| format!("User: {}\nAgent: {}", t.user_input, t.agent_output))
            .collect::<Vec<_>>()
            .join("\n---\n");

        // Use the LLM to summarize instead of truncating
        let compressed = match self.summarize_text(&text).await {
            Ok(summary) => summary,
            Err(e) => {
                warn!(error = %e, "LLM summarization failed — falling back to truncation");
                if text.len() > 2000 {
                    format!(
                        "{}...[truncated {} chars]",
                        &text[..2000],
                        text.len() - 2000
                    )
                } else {
                    text
                }
            }
        };

        let mut summary = self.rolling_summary.write().await;
        if !summary.is_empty() {
            summary.push_str("\n---\n");
        }
        summary.push_str(&compressed);

        info!(
            compressed_turns = to_compress.len(),
            remaining_turns = buffer.len(),
            "Turns compressed into rolling summary"
        );
        Ok(())
    }

    /// Use the summarizer LLM to produce a concise summary of conversation turns.
    async fn summarize_text(&self, text: &str) -> Result<String> {
        let messages = vec![
            ChatMessage {
                role: Role::System,
                content: "You are a precise summarizer. Condense the following conversation turns into a concise summary that preserves all key facts, decisions, and outcomes. Respond with valid JSON: {\"reasoning\": null, \"action\": {\"type\": \"ReturnResult\", \"data\": null, \"summary\": \"YOUR SUMMARY HERE\"}}".into(),
            },
            ChatMessage {
                role: Role::User,
                content: text.to_string(),
            },
        ];

        let response = self.summarizer.generate(&messages, &[], 0.1).await?;

        match &response.message.action {
            AgentAction::ReturnResult { summary, .. } => Ok(summary.clone()),
            _ => {
                // Fallback: use raw text if the LLM didn't follow the schema
                Ok(response.raw_text)
            }
        }
    }

    /// Assemble the context block that gets injected into the LLM prompt.
    pub async fn get_context_block(&self, current_query: &str) -> Result<String> {
        let summary = self.rolling_summary.read().await;
        let buffer = self.turn_buffer.read().await;

        let recent_turns: String = buffer
            .iter()
            .rev()
            .take(30) // last 30 turns
            .rev()
            .map(|t| format!("User: {}\nAgent: {}", t.user_input, t.agent_output))
            .collect::<Vec<_>>()
            .join("\n");

        // Semantic search from LanceDB
        let semantic_hits = self
            .search_similar(current_query, 3)
            .await
            .unwrap_or_default();

        let mut block = String::new();
        if !summary.is_empty() {
            block.push_str("## Conversation History (Compressed)\n");
            block.push_str(&summary);
            block.push_str("\n\n");
        }
        if !semantic_hits.is_empty() {
            block.push_str("## Relevant Past Context\n");
            for hit in &semantic_hits {
                block.push_str(hit);
                block.push_str("\n---\n");
            }
            block.push('\n');
        }
        if !recent_turns.is_empty() {
            block.push_str("## Recent Turns\n");
            block.push_str(&recent_turns);
        }

        Ok(block)
    }

    pub async fn buffer_token_count(&self) -> usize {
        let buffer = self.turn_buffer.read().await;
        buffer.iter().map(|t| t.token_count).sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{AgentAction, AgentMessage, ChatMessage, ToolDefinition};
    use crate::traits::{LlmResponse, TokenUsage};
    use async_trait::async_trait;

    struct MockEmbedder;
    #[async_trait]
    impl EmbeddingProvider for MockEmbedder {
        fn dimension(&self) -> usize {
            4
        }
        async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
            // For testing, just return a sequence of 1.0s to simulate an exact match
            Ok(texts.iter().map(|_| vec![1.0, 1.0, 1.0, 1.0]).collect())
        }
    }

    struct MockSummarizer;
    #[async_trait]
    impl LlmProvider for MockSummarizer {
        fn name(&self) -> &str {
            "mock_summarizer"
        }
        async fn generate(
            &self,
            _m: &[ChatMessage],
            _t: &[ToolDefinition],
            _temp: f32,
        ) -> Result<LlmResponse> {
            Ok(LlmResponse {
                message: AgentMessage::new_root(
                    1,
                    AgentAction::ReturnResult {
                        data: None,
                        summary: "".into(),
                    },
                ),
                usage: TokenUsage {
                    prompt_tokens: 0,
                    completion_tokens: 0,
                },
                raw_text: "".to_string(),
            })
        }
        fn max_context_window(&self) -> usize {
            8000
        }
        fn count_tokens(&self, _text: &str) -> usize {
            10
        }
    }

    #[tokio::test]
    async fn test_jaccard_command_caching() {
        let counter = Arc::new(TokenCounter::new("test"));
        let embedder = Arc::new(MockEmbedder);
        let summarizer = Arc::new(MockSummarizer);

        // LanceDB allows 'memory://' for ephemeral in-memory storage
        let mem = HybridMemory::new("memory://", counter, 1000, embedder, summarizer)
            .await
            .unwrap();

        // 1. Initial state: cache is empty
        let miss = mem.find_cached_command("what is the date").await.unwrap();
        assert!(miss.is_none(), "Cache should be empty initially");

        // 2. Populate the cache
        mem.cache_command("what is the date", "date").await.unwrap();

        // 3. Exact same query => HIT (Jaccard = 1.0)
        let hit = mem.find_cached_command("what is the date").await.unwrap();
        assert!(hit.is_some(), "Exact same query should cache-hit");
        assert_eq!(hit.unwrap(), "date");

        // 4. Completely different query => MISS (low Jaccard)
        let miss2 = mem
            .find_cached_command("list all files starting with R")
            .await
            .unwrap();
        assert!(miss2.is_none(), "Unrelated query should not cache-hit");
    }
}
