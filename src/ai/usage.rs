//! Append-only token/cost accounting for Gemini calls.
//!
//! One JSON line per model call in `<data_dir>/yututui/ai_usage.jsonl`, so spend and latency
//! are observable (and tunable) without a database — mirroring the app's other JSON stores.
//! Best-effort: any I/O or serialize failure is logged at `warn` and swallowed. Usage logging
//! must never disrupt playback.

use std::path::PathBuf;

use serde::Serialize;

use crate::ai::GeminiModel;
use crate::ai::client::UsageMetadata;
use crate::util::safe_fs;

/// USD per 1M (input, output) tokens, standard tier — verified June 2026. `Latest` aliases the
/// current Flash tier, so it is priced as Flash (a conservative over-estimate, never under).
fn price_per_million(model: GeminiModel) -> (f64, f64) {
    match model {
        GeminiModel::FlashLite => (0.10, 0.40),
        GeminiModel::Flash => (0.30, 2.50),
        GeminiModel::Latest => (0.30, 2.50),
    }
}

/// One logged Gemini call.
#[derive(Debug, Clone, Serialize)]
pub struct AiUsageRecord {
    /// Unix seconds.
    pub ts: i64,
    /// `"chat"` | `"rerank"` | `"feedback"` | `"romanize"`.
    pub kind: &'static str,
    /// Resolved API model id (after any fallback).
    pub model: String,
    pub input_tokens: u32,
    /// Billed output = candidate tokens + thinking tokens.
    pub output_tokens: u32,
    pub thought_tokens: u32,
    pub cached_tokens: u32,
    pub total_tokens: u32,
    pub latency_ms: u64,
    /// Whether the response parsed into usable structured output (always `true` for chat).
    pub valid_json: bool,
    pub picked_count: usize,
    /// Whether this call ended up degrading to a local/no-op fallback.
    pub used_fallback: bool,
    pub est_cost_usd: f64,
}

impl AiUsageRecord {
    pub fn new(
        kind: &'static str,
        model: GeminiModel,
        usage: Option<&UsageMetadata>,
        latency_ms: u64,
        valid_json: bool,
        picked_count: usize,
        used_fallback: bool,
    ) -> Self {
        let u = usage.cloned().unwrap_or_default();
        let input = u.prompt_token_count;
        let output = u
            .candidates_token_count
            .saturating_add(u.thoughts_token_count);
        let (in_price, out_price) = price_per_million(model);
        let est_cost_usd = (input as f64 / 1e6) * in_price + (output as f64 / 1e6) * out_price;
        Self {
            ts: crate::signals::unix_now(),
            kind,
            model: model.api_id().to_owned(),
            input_tokens: input,
            output_tokens: output,
            thought_tokens: u.thoughts_token_count,
            cached_tokens: u.cached_content_token_count,
            total_tokens: u.total_token_count,
            latency_ms,
            valid_json,
            picked_count,
            used_fallback,
            est_cost_usd,
        }
    }
}

fn log_path() -> Option<PathBuf> {
    directories::ProjectDirs::from("", "", "yututui").map(|d| d.data_dir().join("ai_usage.jsonl"))
}

/// Append one record as a JSON line. Best-effort; never panics.
pub fn append(record: &AiUsageRecord) {
    let Some(path) = log_path() else {
        return;
    };
    let line = match serde_json::to_string(record) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(error = %e, "ai usage: serialize failed");
            return;
        }
    };
    if let Err(e) = safe_fs::append_private_jsonl(&path, &line) {
        tracing::warn!(error = %e, "ai usage: append failed");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cost_estimate_uses_input_and_output_prices() {
        let usage = UsageMetadata {
            prompt_token_count: 1_000_000,
            candidates_token_count: 1_000_000,
            total_token_count: 2_000_000,
            thoughts_token_count: 0,
            cached_content_token_count: 0,
        };
        let r = AiUsageRecord::new(
            "rerank",
            GeminiModel::FlashLite,
            Some(&usage),
            12,
            true,
            5,
            false,
        );
        // 1M input * $0.10 + 1M output * $0.40 = $0.50.
        assert!((r.est_cost_usd - 0.50).abs() < 1e-9);
        assert_eq!(r.input_tokens, 1_000_000);
        assert_eq!(r.output_tokens, 1_000_000);
        assert_eq!(r.model, "gemini-2.5-flash-lite");
    }

    #[test]
    fn thinking_tokens_count_as_output() {
        let usage = UsageMetadata {
            prompt_token_count: 0,
            candidates_token_count: 10,
            total_token_count: 40,
            thoughts_token_count: 30,
            cached_content_token_count: 0,
        };
        let r = AiUsageRecord::new("chat", GeminiModel::Flash, Some(&usage), 0, true, 0, false);
        assert_eq!(r.output_tokens, 40);
        assert_eq!(r.thought_tokens, 30);
    }

    #[test]
    fn missing_usage_is_zero_cost() {
        let r = AiUsageRecord::new("rerank", GeminiModel::FlashLite, None, 0, false, 0, true);
        assert_eq!(r.est_cost_usd, 0.0);
        assert_eq!(r.total_tokens, 0);
    }
}
