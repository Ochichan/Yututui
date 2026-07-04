//! The Gemini REST client: request/response models and a single `generate()` call with
//! transport-level retry (429 / 5xx / network) and typed errors.
//!
//! Wire facts that are easy to get wrong, all enforced here:
//! - Auth is the **`x-goog-api-key` header**, never `?key=` (keeps the key out of URLs and
//!   request logs). The header value is marked sensitive.
//! - **Every** request struct is `#[serde(rename_all = "camelCase")]` — Gemini 400s on
//!   snake_case keys (`systemInstruction`, `functionDeclarations`, `maxOutputTokens`, …).
//!   A unit test asserts the serialized keys to catch a regression the compiler can't.
//! - [`Part`] keeps unknown fields via `#[serde(flatten)]` so echoing a model turn back
//!   preserves `thoughtSignature`; dropping it makes the next turn 400 on thinking models.
//!
//! Model fallback is *not* here — it lives in the actor, which alone knows whether a
//! side-effecting tool already ran (after which a retry on a different model is unsafe).

use std::fmt;
use std::time::Duration;

use reqwest::header::{HeaderValue, RETRY_AFTER};
use serde::{Deserialize, Serialize};
use tokio::time::sleep;

use super::GeminiModel;
use crate::util::{http, sanitize};

/// Per-request transport retries (does not include the first attempt).
const MAX_RETRIES: u32 = 3;
/// Cap on error-body text kept in messages/logs.
const ERR_BODY_CAP: usize = 200;
const RESPONSE_BODY_MAX: usize = 4 * 1024 * 1024;
const ERROR_BODY_MAX: usize = 64 * 1024;

// --- Request models ---------------------------------------------------------

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GenerateContentRequest {
    pub contents: Vec<Content>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system_instruction: Option<Content>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<Tool>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub generation_config: Option<GenerationConfig>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Tool {
    /// The 13 tool schemas, built in `tools.rs` as JSON values.
    pub function_declarations: Vec<serde_json::Value>,
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GenerationConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f64>,
    /// `"application/json"` to force a JSON response (the structured-output path).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_mime_type: Option<String>,
    /// An OpenAPI-subset schema constraining the JSON response shape.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_schema: Option<serde_json::Value>,
    /// Disables/limits "thinking" tokens. `thinking_budget: 0` turns thinking off — important on
    /// 2.5 Flash (defaults to dynamic thinking ON) so the budget isn't drained from output.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking_config: Option<ThinkingConfig>,
}

/// `generationConfig.thinkingConfig`. `thinking_budget` is `i32` so `-1` ("dynamic") is
/// representable, though the reranker pins it to `0` (off).
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ThinkingConfig {
    pub thinking_budget: i32,
}

// --- Shared / response models -----------------------------------------------

/// A turn of conversation. Appears in both the request and the response, so it derives
/// both `Serialize` and `Deserialize`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Content {
    /// "user" or "model". Omitted for `systemInstruction`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    pub parts: Vec<Part>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Part {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub function_call: Option<FunctionCall>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub function_response: Option<FunctionResponse>,
    /// Unknown fields (notably `thoughtSignature`) preserved verbatim across an echo.
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionCall {
    pub name: String,
    #[serde(default)]
    pub args: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionResponse {
    pub name: String,
    /// Gemini expects an object here; we wrap tool output as `{ "result": <value> }`.
    pub response: serde_json::Value,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GenerateContentResponse {
    #[serde(default)]
    pub candidates: Vec<Candidate>,
    #[serde(default)]
    pub prompt_feedback: Option<PromptFeedback>,
    /// Token accounting for cost logging (see [`crate::ai::usage`]). Absent on error bodies.
    #[serde(default)]
    pub usage_metadata: Option<UsageMetadata>,
}

/// Gemini's per-response token counts. All optional/defaulted — the API omits zero fields and
/// older responses may not carry every counter.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageMetadata {
    #[serde(default)]
    pub prompt_token_count: u32,
    #[serde(default)]
    pub candidates_token_count: u32,
    #[serde(default)]
    pub total_token_count: u32,
    /// Thinking tokens (0 when thinking is off, as for the rerank path).
    #[serde(default)]
    pub thoughts_token_count: u32,
    /// Tokens served from a context cache (billed at a discount).
    #[serde(default)]
    pub cached_content_token_count: u32,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Candidate {
    pub content: Option<Content>,
    #[serde(default)]
    pub finish_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptFeedback {
    #[serde(default)]
    pub block_reason: Option<String>,
}

impl Part {
    pub fn text(s: impl Into<String>) -> Self {
        Part {
            text: Some(s.into()),
            ..Default::default()
        }
    }

    pub fn function_response(name: impl Into<String>, result: serde_json::Value) -> Self {
        Part {
            function_response: Some(FunctionResponse {
                name: name.into(),
                response: serde_json::json!({ "result": result }),
            }),
            ..Default::default()
        }
    }
}

impl Content {
    pub fn user(parts: Vec<Part>) -> Self {
        Content {
            role: Some("user".to_owned()),
            parts,
        }
    }

    /// Concatenated text across all `text` parts.
    pub fn joined_text(&self) -> String {
        self.parts
            .iter()
            .filter_map(|p| p.text.as_deref())
            .collect::<Vec<_>>()
            .join("")
    }

    /// All `functionCall` parts in order.
    pub fn function_calls(&self) -> Vec<&FunctionCall> {
        self.parts
            .iter()
            .filter_map(|p| p.function_call.as_ref())
            .collect()
    }
}

impl GenerateContentResponse {
    /// The model's content for the (single) candidate, if present.
    pub fn content(&self) -> Option<&Content> {
        self.candidates.first()?.content.as_ref()
    }

    pub fn finish_reason(&self) -> Option<&str> {
        self.candidates.first()?.finish_reason.as_deref()
    }

    pub fn block_reason(&self) -> Option<&str> {
        self.prompt_feedback.as_ref()?.block_reason.as_deref()
    }

    pub fn usage(&self) -> Option<&UsageMetadata> {
        self.usage_metadata.as_ref()
    }
}

// --- Errors -----------------------------------------------------------------

#[derive(Debug)]
pub enum GeminiError {
    /// 401/403 — bad or unauthorized key. Not retried; no fallback.
    Auth,
    /// 404 — the model id is gone. The actor may try a fallback model.
    ModelNotFound,
    /// 429 after retries exhausted.
    RateLimited,
    /// 5xx after retries exhausted.
    Server(String),
    /// Any other non-success status.
    Http(String),
    /// Transport failure / timeout.
    Network(String),
    /// Response body didn't decode.
    Decode(String),
}

impl fmt::Display for GeminiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GeminiError::Auth => write!(f, "API key rejected (check your Gemini key)"),
            GeminiError::ModelNotFound => write!(f, "model not found"),
            GeminiError::RateLimited => write!(f, "rate limited — try again shortly"),
            GeminiError::Server(s) => write!(f, "Gemini server error: {s}"),
            GeminiError::Http(s) => write!(f, "{s}"),
            GeminiError::Network(s) => write!(f, "network error: {s}"),
            GeminiError::Decode(s) => write!(f, "could not parse response: {s}"),
        }
    }
}

impl std::error::Error for GeminiError {}

impl GeminiError {
    /// Whether trying a fallback model could help (vs. a key/usage problem that won't).
    pub fn is_model_fallbackable(&self) -> bool {
        matches!(
            self,
            GeminiError::ModelNotFound | GeminiError::Server(_) | GeminiError::RateLimited
        )
    }

    /// A short, human-facing description for the transcript / overlay. `Display` stays
    /// verbose for logs; this one never dumps a raw response body at the user — an HTTP
    /// error is mined for Google's own `error.message` (with the invalid-key case folded
    /// into the key guidance) and firmly truncated.
    pub fn user_message(&self) -> String {
        let key_help = || {
            crate::t!(
                "The Gemini API key was rejected — check it in Settings.",
                "Gemini API 키가 거부됐어요 — 설정에서 키를 확인해 주세요."
            )
            .to_owned()
        };
        match self {
            GeminiError::Auth => key_help(),
            GeminiError::ModelNotFound => crate::t!(
                "That Gemini model isn't available — pick another in Settings.",
                "이 Gemini 모델은 사용할 수 없어요 — 설정에서 다른 모델을 골라 주세요."
            )
            .to_owned(),
            GeminiError::RateLimited => crate::t!(
                "Gemini is rate-limiting us — try again in a moment.",
                "요청이 너무 잦아요 — 잠시 후 다시 시도해 주세요."
            )
            .to_owned(),
            GeminiError::Server(_) => crate::t!(
                "Gemini had a server hiccup — try again shortly.",
                "Gemini 서버 오류예요 — 잠시 후 다시 시도해 주세요."
            )
            .to_owned(),
            GeminiError::Network(_) => crate::t!(
                "Couldn't reach Gemini — check your network.",
                "Gemini에 연결하지 못했어요 — 네트워크를 확인해 주세요."
            )
            .to_owned(),
            GeminiError::Decode(_) => crate::t!(
                "Couldn't parse Gemini's reply.",
                "Gemini 응답을 해석하지 못했어요."
            )
            .to_owned(),
            GeminiError::Http(raw) => {
                // Google reports a bad key as HTTP 400 API_KEY_INVALID, not 401/403.
                if raw.contains("API_KEY_INVALID") || raw.contains("API key not valid") {
                    return key_help();
                }
                let rejected =
                    crate::t!("Gemini rejected the request", "Gemini가 요청을 거부했어요");
                match http_error_detail(raw) {
                    Some(detail) => format!("{rejected} ({detail})"),
                    None => format!("{rejected}."),
                }
            }
        }
    }
}

/// Pull Google's `error.message` out of an `HTTP <code>: <json body>` string, truncated
/// to one short parenthetical. `None` when the body isn't the expected JSON shape.
fn http_error_detail(raw: &str) -> Option<String> {
    let body = &raw[raw.find('{')?..];
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    let msg = v.get("error")?.get("message")?.as_str()?.trim();
    if msg.is_empty() {
        return None;
    }
    let mut out: String = msg.chars().take(100).collect();
    if out.len() < msg.len() {
        out.push('…');
    }
    Some(out)
}

// --- Client -----------------------------------------------------------------

/// A Gemini REST client. Deliberately does NOT derive `Debug` — the key must never be
/// printed.
pub struct GeminiClient {
    http: reqwest::Client,
    /// The `x-goog-api-key` value, marked sensitive.
    key: HeaderValue,
}

impl GeminiClient {
    /// Build a client. Fails only if the key has bytes invalid for an HTTP header.
    pub fn new(api_key: &str) -> Result<Self, GeminiError> {
        let mut key = HeaderValue::from_str(api_key).map_err(|_| GeminiError::Auth)?;
        key.set_sensitive(true);
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|e| GeminiError::Network(e.to_string()))?;
        Ok(Self { http, key })
    }

    /// One `generateContent` call against `model`, retrying transient failures.
    pub async fn generate(
        &self,
        model: GeminiModel,
        req: &GenerateContentRequest,
    ) -> Result<GenerateContentResponse, GeminiError> {
        let url = format!(
            "https://generativelanguage.googleapis.com/v1beta/models/{}:generateContent",
            model.api_id()
        );
        let mut attempt = 0u32;
        loop {
            let send = self
                .http
                .post(&url)
                .header("x-goog-api-key", self.key.clone())
                .json(req)
                .send()
                .await;

            match send {
                Ok(resp) => {
                    let status = resp.status();
                    if status.is_success() {
                        return http::json_limited::<GenerateContentResponse>(
                            resp,
                            RESPONSE_BODY_MAX,
                        )
                        .await
                        .map_err(|e| {
                            GeminiError::Decode(sanitize::sanitize_error_text(e.to_string()))
                        });
                    }
                    let code = status.as_u16();
                    let retry_after = resp
                        .headers()
                        .get(RETRY_AFTER)
                        .and_then(|v| v.to_str().ok())
                        .and_then(|s| s.trim().parse::<u64>().ok());
                    let body = http::read_response_limited(resp, ERROR_BODY_MAX)
                        .await
                        .ok()
                        .and_then(|b| String::from_utf8(b).ok())
                        .map(sanitize::sanitize_error_text)
                        .unwrap_or_default();
                    match code {
                        401 | 403 => return Err(GeminiError::Auth),
                        404 => return Err(GeminiError::ModelNotFound),
                        429 => {
                            if attempt >= MAX_RETRIES {
                                return Err(GeminiError::RateLimited);
                            }
                            let secs = retry_after.unwrap_or_else(|| 1u64 << attempt); // 1/2/4
                            sleep(Duration::from_secs(secs)).await;
                        }
                        500..=599 => {
                            if attempt >= MAX_RETRIES {
                                return Err(GeminiError::Server(truncate(&body)));
                            }
                            sleep(server_backoff(attempt)).await;
                        }
                        _ => {
                            return Err(GeminiError::Http(format!(
                                "HTTP {code}: {}",
                                truncate(&body)
                            )));
                        }
                    }
                }
                Err(e) => {
                    if attempt >= MAX_RETRIES {
                        return Err(GeminiError::Network(e.to_string()));
                    }
                    sleep(server_backoff(attempt)).await;
                }
            }
            attempt += 1;
        }
    }
}

/// 5xx/network backoff: 0.6 / 1.2 / 2.4 s.
fn server_backoff(attempt: u32) -> Duration {
    Duration::from_millis(600 * (1u64 << attempt))
}

fn truncate(s: &str) -> String {
    let s = s.trim();
    if s.len() <= ERR_BODY_CAP {
        s.to_owned()
    } else {
        // ERR_BODY_CAP is a *byte* cap; snap it down to a char boundary before slicing.
        // Gemini error bodies are multilingual (localized messages, echoed user content),
        // so a multi-byte UTF-8 codepoint can straddle byte 200 — a raw `&s[..200]` would
        // then panic with "byte index 200 is not a char boundary".
        let end = s.floor_char_boundary(ERR_BODY_CAP);
        format!("{}…", &s[..end])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_caps_at_char_boundary_without_panicking() {
        // Short bodies pass through verbatim (after trimming).
        assert_eq!(truncate("  short error  "), "short error");

        // A long body whose byte cap lands inside a multi-byte codepoint must not panic
        // (regression: a raw `&s[..200]` slice panicked on a non-ASCII Gemini error body).
        // 199 ASCII bytes then a 3-byte Korean codepoint straddling byte 200.
        let body = format!("{}{}", "x".repeat(199), "가".repeat(10));
        let out = truncate(&body);
        assert!(out.ends_with('…'));
        // Snapped *down* to the last char boundary ≤ 200, i.e. the 199 ASCII bytes.
        assert_eq!(out, format!("{}…", "x".repeat(199)));

        // An all-multibyte body longer than the cap is likewise safe.
        let cjk = "가".repeat(100); // 300 bytes
        let out = truncate(&cjk);
        assert!(out.ends_with('…'));
        assert!(out.len() <= ERR_BODY_CAP + '…'.len_utf8());
    }

    #[test]
    fn user_messages_stay_short_and_key_failures_guide_to_settings() {
        // Google reports a bad key as HTTP 400 API_KEY_INVALID — fold into key guidance.
        let e = GeminiError::Http(
            "HTTP 400: {\"error\":{\"code\":400,\"message\":\"API key not valid. Please pass a valid API key.\",\"status\":\"INVALID_ARGUMENT\"}}"
                .to_owned(),
        );
        let msg = e.user_message();
        assert!(msg.contains("Settings"), "got: {msg}");
        assert!(!msg.contains('{'), "no raw JSON reaches the user: {msg}");

        // Other HTTP errors surface Google's own message, sans the JSON shell.
        let e = GeminiError::Http(
            "HTTP 400: {\"error\":{\"message\":\"Invalid JSON payload received.\"}}".to_owned(),
        );
        let msg = e.user_message();
        assert!(msg.contains("Invalid JSON payload received."), "got: {msg}");
        assert!(!msg.contains('{'));

        // An unparseable body degrades to the generic one-liner, still no dump.
        let e = GeminiError::Http("HTTP 418: teapot".to_owned());
        assert_eq!(e.user_message(), "Gemini rejected the request.");

        // A huge Google message truncates to one short parenthetical.
        let long = "x".repeat(500);
        let e = GeminiError::Http(format!(
            "HTTP 400: {{\"error\":{{\"message\":\"{long}\"}}}}"
        ));
        let msg = e.user_message();
        assert!(
            msg.chars().count() < 140,
            "got {} chars",
            msg.chars().count()
        );
        assert!(msg.contains('…'));

        // The non-HTTP variants are fixed one-liners.
        assert!(GeminiError::Auth.user_message().contains("Settings"));
        assert!(
            !GeminiError::Network("io".into())
                .user_message()
                .contains("io")
        );
    }

    #[test]
    fn request_serializes_with_camelcase_keys() {
        let req = GenerateContentRequest {
            contents: vec![Content::user(vec![Part::text("hi")])],
            system_instruction: Some(Content {
                role: None,
                parts: vec![Part::text("be brief")],
            }),
            tools: Some(vec![Tool {
                function_declarations: vec![serde_json::json!({"name": "x"})],
            }]),
            generation_config: Some(GenerationConfig {
                temperature: Some(0.7),
                max_output_tokens: Some(1024),
                response_mime_type: Some("application/json".to_owned()),
                response_schema: Some(serde_json::json!({ "type": "object" })),
                thinking_config: Some(ThinkingConfig { thinking_budget: 0 }),
                ..Default::default()
            }),
        };
        let v = serde_json::to_value(&req).unwrap();
        // The keys Gemini insists on being camelCase.
        assert!(v.get("systemInstruction").is_some());
        assert!(v.get("generationConfig").is_some());
        assert!(v["generationConfig"].get("maxOutputTokens").is_some());
        assert!(v["generationConfig"].get("responseMimeType").is_some());
        assert!(v["generationConfig"].get("responseSchema").is_some());
        assert_eq!(v["generationConfig"]["thinkingConfig"]["thinkingBudget"], 0);
        assert!(v["tools"][0].get("functionDeclarations").is_some());
        // snake_case must NOT appear.
        assert!(v.get("system_instruction").is_none());
        assert!(v["generationConfig"].get("max_output_tokens").is_none());
        assert!(v["generationConfig"].get("response_mime_type").is_none());
    }

    #[test]
    fn part_roundtrips_and_preserves_thought_signature() {
        // A model turn carrying an opaque thoughtSignature must survive a parse → re-emit.
        let raw = serde_json::json!({
            "role": "model",
            "parts": [
                { "text": "ok", "thoughtSignature": "abc123" },
                { "functionCall": { "name": "play_music", "args": { "query": "lofi" } } }
            ]
        });
        let content: Content = serde_json::from_value(raw).unwrap();
        assert_eq!(content.function_calls().len(), 1);
        assert_eq!(content.function_calls()[0].name, "play_music");
        let back = serde_json::to_value(&content).unwrap();
        assert_eq!(back["parts"][0]["thoughtSignature"], "abc123");
        // functionCall key stays camelCase.
        assert!(back["parts"][1].get("functionCall").is_some());
    }

    #[test]
    fn function_response_wraps_result() {
        let p = Part::function_response("get_queue", serde_json::json!({ "len": 3 }));
        let v = serde_json::to_value(&p).unwrap();
        assert_eq!(v["functionResponse"]["name"], "get_queue");
        assert_eq!(v["functionResponse"]["response"]["result"]["len"], 3);
    }

    #[test]
    fn response_parses_finish_reason_and_text() {
        let raw = serde_json::json!({
            "candidates": [{
                "content": { "role": "model", "parts": [{ "text": "hello" }] },
                "finishReason": "STOP"
            }]
        });
        let resp: GenerateContentResponse = serde_json::from_value(raw).unwrap();
        assert_eq!(resp.finish_reason(), Some("STOP"));
        assert_eq!(resp.content().unwrap().joined_text(), "hello");
    }
}
