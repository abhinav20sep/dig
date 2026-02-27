use governor::{Quota, RateLimiter, state::direct::NotKeyed, state::InMemoryState};
use std::num::NonZeroU32;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use anyhow::{Result, anyhow, bail};
use tracing::{warn, info};

// ── Tier 1: Global Token Governor ────────────────────────────────

/// Controls both throughput (tokens/minute via leaky bucket) and
/// cumulative spend (total tokens via atomic counter).
#[derive(Clone)]
pub struct TokenGovernor {
    limiter: Arc<RateLimiter<NotKeyed, InMemoryState, governor::clock::DefaultClock>>,
    total_used: Arc<AtomicU64>,
    max_budget: u64,
}

impl TokenGovernor {
    pub fn new(tokens_per_minute: u32, max_budget: u64) -> Result<Self> {
        let capacity = NonZeroU32::new(tokens_per_minute)
            .ok_or_else(|| anyhow!("tokens_per_minute must be > 0"))?;

        let quota = Quota::per_minute(capacity);
        let limiter = Arc::new(RateLimiter::direct(quota));

        Ok(Self {
            limiter,
            total_used: Arc::new(AtomicU64::new(0)),
            max_budget,
        })
    }

    /// Record token usage and enforce both rate-limit and cumulative budget.
    /// Returns an error if budget is exhausted.
    pub async fn consume(&self, tokens: u64) -> Result<()> {
        // ── Budget gate ──
        let prev = self.total_used.fetch_add(tokens, Ordering::SeqCst);
        let new_total = prev + tokens;
        if new_total > self.max_budget {
            self.total_used.fetch_sub(tokens, Ordering::SeqCst); // roll back
            bail!(
                "Token budget exhausted: used {} + requested {} > budget {}",
                prev, tokens, self.max_budget
            );
        }

        // ── Rate-limit gate ──
        // Clamp to u32 for the rate limiter (single request is unlikely > 4B tokens)
        let clamped = tokens.min(u32::MAX as u64) as u32;
        if let Some(cells) = NonZeroU32::new(clamped) {
            self.limiter
                .until_n_ready(cells)
                .await
                .map_err(|e| anyhow!("Rate limiter burst exceeded: {:?}", e))?;
        }

        info!(total = new_total, budget = self.max_budget, "tokens consumed");
        Ok(())
    }

    /// How many tokens have been used so far.
    pub fn total_used(&self) -> u64 {
        self.total_used.load(Ordering::SeqCst)
    }

    /// How many tokens remain in the budget.
    pub fn remaining(&self) -> u64 {
        self.max_budget.saturating_sub(self.total_used())
    }

    /// Check if the budget is exhausted without consuming.
    pub fn is_exhausted(&self) -> bool {
        self.total_used() >= self.max_budget
    }
}

// ── Tier 2: Recursion & Reasoning Budget ─────────────────────────

/// Validates TTL and turn-count limits before an agent step proceeds.
pub struct BudgetEnforcer {
    pub max_ttl: u32,
    pub max_turns: u32,
}

impl BudgetEnforcer {
    pub fn new(max_ttl: u32, max_turns: u32) -> Self {
        Self { max_ttl, max_turns }
    }

    /// Returns `Err` if the agent should stop (TTL exhausted or turn limit hit).
    pub fn check(&self, ttl: u32, turn_count: u32) -> Result<()> {
        if ttl == 0 {
            warn!(ttl, "TTL exhausted — forcing GiveUp");
            bail!("TTL exhausted: agent recursion depth limit reached");
        }
        if turn_count >= self.max_turns {
            warn!(turn_count, max = self.max_turns, "Turn budget exhausted");
            bail!(
                "Turn budget exhausted: {} / {} turns used",
                turn_count, self.max_turns
            );
        }
        Ok(())
    }
}

// ── Token Counting ───────────────────────────────────────────────

/// Configurable token counter.  
/// Falls back to a simple heuristic (chars / 4) if the requested encoding is unavailable.
pub struct TokenCounter {
    bpe: Option<tiktoken_rs::CoreBPE>,
}

impl TokenCounter {
    /// Create a counter for a known encoding name (e.g. "cl100k_base", "o200k_base").
    pub fn new(encoding: &str) -> Self {
        let bpe = match encoding {
            "cl100k_base" => tiktoken_rs::cl100k_base().ok(),
            "o200k_base" => tiktoken_rs::o200k_base().ok(),
            _ => {
                warn!(encoding, "Unknown tiktoken encoding, using heuristic counter");
                None
            }
        };
        Self { bpe }
    }

    pub fn count(&self, text: &str) -> usize {
        match &self.bpe {
            Some(bpe) => bpe.encode_with_special_tokens(text).len(),
            None => text.len() / 4, // rough heuristic
        }
    }
}
