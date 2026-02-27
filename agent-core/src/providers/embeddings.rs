use async_trait::async_trait;
use anyhow::{Result, Context, bail};
use reqwest::{Client, header};
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

/// Trait for embedding providers (OpenAI, local models, etc.)
#[async_trait]
pub trait EmbeddingProvider: Send + Sync {
    /// Embed one or more texts into vectors.
    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>>;
    /// Dimensionality of the embedding vectors.
    fn dimension(&self) -> usize;
}

/// OpenAI-compatible embeddings client.
pub struct OpenAiEmbeddings {
    client: Client,
    api_base: String,
    model: String,
    dim: usize,
}

#[derive(Serialize)]
struct EmbeddingRequest<'a> {
    input: &'a [String],
    model: &'a str,
}

#[derive(Deserialize, Debug)]
struct EmbeddingResponse {
    data: Vec<EmbeddingData>,
    #[allow(dead_code)]
    usage: Option<EmbeddingUsage>,
}

#[derive(Deserialize, Debug)]
struct EmbeddingData {
    embedding: Vec<f32>,
}

#[derive(Deserialize, Debug)]
struct EmbeddingUsage {
    #[allow(dead_code)]
    total_tokens: u64,
}

impl OpenAiEmbeddings {
    pub fn new(api_key: String, api_base: String, model: String) -> Result<Self> {
        let mut headers = header::HeaderMap::new();
        let mut auth_val = header::HeaderValue::from_str(&format!("Bearer {}", api_key))
            .context("Invalid characters in API key")?;
        auth_val.set_sensitive(true);
        headers.insert(header::AUTHORIZATION, auth_val);

        let client = Client::builder()
            .default_headers(headers)
            .build()?;

        // Map model name to dimension
        let dim = match model.as_str() {
            "text-embedding-3-small" => 1536,
            "text-embedding-3-large" => 3072,
            "text-embedding-ada-002" => 1536,
            _ => 1536, // fallback
        };

        Ok(Self { client, api_base, model, dim })
    }
}

#[async_trait]
impl EmbeddingProvider for OpenAiEmbeddings {
    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(vec![]);
        }

        // OpenAI allows max 2048 inputs per call; batch if needed
        let mut all_embeddings = Vec::with_capacity(texts.len());
        for chunk in texts.chunks(2048) {
            let req = EmbeddingRequest {
                input: chunk,
                model: &self.model,
            };

            let url = format!("{}/embeddings", self.api_base);
            let resp = self.client.post(&url).json(&req).send().await?;

            if !resp.status().is_success() {
                let status = resp.status();
                let error_text = resp.text().await.unwrap_or_default();
                bail!("Embedding API error ({}): {}", status, error_text);
            }

            let body: EmbeddingResponse = resp.json().await?;
            debug!(count = body.data.len(), "Embeddings received");

            for item in body.data {
                all_embeddings.push(item.embedding);
            }
        }

        if all_embeddings.len() != texts.len() {
            warn!(
                expected = texts.len(),
                got = all_embeddings.len(),
                "Embedding count mismatch"
            );
        }

        Ok(all_embeddings)
    }

    fn dimension(&self) -> usize {
        self.dim
    }
}
