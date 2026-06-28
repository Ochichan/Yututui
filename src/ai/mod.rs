//! The AI assistant: a multi-turn Gemini function-calling agent that drives playback.
//!
//! Mirrors `youtube-music-cli`'s LLM service, adapted to this app's TEA architecture: the
//! actor can't touch `App`, so tool side-effects flow back as [`crate::app::Msg`]s that
//! `update()` applies. The model invokes tools (search, play, queue, radio, playlists);
//! resolves run inside the actor via yt-dlp; mutations are reported back as intents.
//!
//! The loop (`converse`):
//! 1. Send the conversation + tool schemas to Gemini.
//! 2. If the reply has `functionCall` parts, **echo the model turn verbatim** (preserving
//!    `thoughtSignature`), execute each tool, append the results as a new `user` turn, and
//!    loop — up to [`MAX_ROUNDS`].
//! 3. Otherwise emit the reply text and stop.
//!
//! Safety rails: a client-side rate limiter; an RAII guard that always clears the
//! thinking spinner; and model fallback that is **disabled once a side-effecting tool has
//! run** (so a retry on another model can't double-apply a playback change).

pub mod client;
pub mod model;
pub mod tools;

pub use model::GeminiModel;

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tokio::time::{sleep, timeout};

use crate::api::Song;
use crate::app::{AiContext, Msg};
use client::{
    Content, GeminiClient, GeminiError, GenerateContentRequest, GenerationConfig, Part,
    ThinkingConfig, Tool,
};

/// Max tool-calling rounds before we give up (matches youtube-music-cli's cap).
const MAX_ROUNDS: usize = 5;
/// Client-side rate limit: at most this many requests per [`RATE_WINDOW`].
const RATE_LIMIT: usize = 15;
const RATE_WINDOW: Duration = Duration::from_secs(60);

/// Reranker wall-clock budget: degrade to the local pick past this (Flash-Lite p99 can spike
/// to several seconds, and we never want a slow rerank to stall the queue top-up).
const RERANK_TIMEOUT: Duration = Duration::from_secs(9);
/// Pure selection task → low variance.
const RERANK_TEMPERATURE: f64 = 0.1;
/// Thinking is off, but a ~12-id JSON array still needs headroom (a 64-token cap truncates to
/// `MAX_TOKENS`).
const RERANK_MAX_TOKENS: u32 = 512;
const RERANK_SYSTEM_PROMPT: &str = "\
You are RadioNext, a JSON-only radio reranker for a music player. You are given the recent \
listening session (a seed track plus the last few played) and a list of candidate tracks, \
each with an id. Rank the candidates into the best radio continuation: keep the flow coherent \
with the seed, avoid repeating the same artist back-to-back, and shape a pleasant arc. Choose \
ONLY from the given candidate ids — never invent or alter an id. Respond with JSON \
{\"ids\":[...]} listing the chosen ids best-first, and nothing else.";

const SYSTEM_PROMPT: &str = "\
You are the built-in music assistant for ytm-tui, a terminal YouTube Music player. You \
control real playback through the provided tools — when the user asks for music, take \
action with tools rather than only describing it. Typically search_tracks first to get \
videoIds, then play_music or add_to_queue with those ids. Keep replies short and \
friendly, and reply in the user's language. Prefer the user's own queue, favorites, and \
playlists when relevant. If a request is ambiguous, make a reasonable choice and proceed. \
Never fabricate tool results or videoIds you haven't seen.";

/// Commands sent to the AI actor.
pub enum AiCmd {
    Ask { prompt: String, context: Box<AiContext> },
    /// One-shot ids-only radio rerank over a local candidate shortlist (the autoplay path).
    Rerank { seed_video_id: String, prompt: String },
    /// Switch the model used for subsequent requests (settings save).
    SetModel(GeminiModel),
}

/// Handle for issuing assistant requests; results return as [`Msg`]s.
pub struct AiHandle {
    tx: UnboundedSender<AiCmd>,
}

impl AiHandle {
    pub fn ask(&self, prompt: String, context: Box<AiContext>) {
        let _ = self.tx.send(AiCmd::Ask { prompt, context });
    }

    /// Kick off a one-shot radio rerank; the result returns as [`Msg::RadioAiPicks`].
    pub fn rerank(&self, seed_video_id: String, prompt: String) {
        let _ = self.tx.send(AiCmd::Rerank { seed_video_id, prompt });
    }

    /// Hot-swap the model for future requests. Ignored if the actor has stopped.
    pub fn set_model(&self, model: GeminiModel) {
        let _ = self.tx.send(AiCmd::SetModel(model));
    }
}

/// Spawn the AI actor. Returns `None` if the key can't form a valid header (treated as
/// "no assistant"); the caller then leaves `ai_available` false.
pub fn spawn(api_key: &str, model: GeminiModel, msg_tx: UnboundedSender<Msg>) -> Option<AiHandle> {
    let client = GeminiClient::new(api_key).ok()?;
    let (tx, rx) = mpsc::unbounded_channel();
    let actor = AiActor { client, model, msg_tx, call_times: VecDeque::new() };
    tokio::spawn(actor.run(rx));
    Some(AiHandle { tx })
}

struct AiActor {
    client: GeminiClient,
    model: GeminiModel,
    msg_tx: UnboundedSender<Msg>,
    /// Timestamps of recent Gemini calls, for the rate limiter.
    call_times: VecDeque<Instant>,
}

/// Clears the thinking spinner whenever a turn ends — including any `?`/early return,
/// since Rust has no `defer`.
struct ThinkingGuard(UnboundedSender<Msg>);

impl Drop for ThinkingGuard {
    fn drop(&mut self) {
        let _ = self.0.send(Msg::AiThinking(false));
    }
}

impl AiActor {
    async fn run(mut self, mut rx: UnboundedReceiver<AiCmd>) {
        while let Some(cmd) = rx.recv().await {
            match cmd {
                AiCmd::Ask { prompt, context } => self.converse(prompt, *context).await,
                AiCmd::Rerank { seed_video_id, prompt } => {
                    self.rerank(seed_video_id, prompt).await
                }
                AiCmd::SetModel(model) => self.model = model,
            }
        }
    }

    /// Throttle to the client-side rate limit, then record the call.
    async fn throttle(&mut self) {
        let now = Instant::now();
        while let Some(&front) = self.call_times.front() {
            if now.duration_since(front) >= RATE_WINDOW {
                self.call_times.pop_front();
            } else {
                break;
            }
        }
        if self.call_times.len() >= RATE_LIMIT
            && let Some(&oldest) = self.call_times.front()
        {
            let wait = RATE_WINDOW.saturating_sub(oldest.elapsed());
            if !wait.is_zero() {
                sleep(wait).await;
            }
            self.call_times.pop_front();
        }
        self.call_times.push_back(Instant::now());
    }

    /// One Gemini call, falling back to a cheaper/older model on a fallbackable error —
    /// but only while no side effect has happened yet.
    async fn generate(
        &mut self,
        req: &GenerateContentRequest,
        model: &mut GeminiModel,
        side_effected: bool,
    ) -> Result<client::GenerateContentResponse, GeminiError> {
        loop {
            self.throttle().await;
            match self.client.generate(*model, req).await {
                Ok(r) => return Ok(r),
                Err(e) => {
                    if !side_effected
                        && e.is_model_fallbackable()
                        && let Some(fb) = model.fallback()
                    {
                        tracing::warn!(from = model.label(), to = fb.label(), error = %e, "AI model fallback");
                        *model = fb;
                        continue;
                    }
                    return Err(e);
                }
            }
        }
    }

    async fn converse(&mut self, prompt: String, ctx: AiContext) {
        let _guard = ThinkingGuard(self.msg_tx.clone());

        // When the UI is set to Korean, steer replies to Korean explicitly — the base prompt
        // only says "reply in the user's language", which leaves English prompts ambiguous.
        let system_text = if crate::i18n::is_korean() {
            format!("{SYSTEM_PROMPT}\n\nRespond in Korean (한국어) regardless of the language the user writes in.")
        } else {
            SYSTEM_PROMPT.to_owned()
        };
        let system = Content { role: None, parts: vec![Part::text(system_text)] };
        let decls = tools::declarations();
        let gen_cfg = GenerationConfig {
            temperature: Some(0.7),
            max_output_tokens: Some(2048),
            ..Default::default()
        };
        let first = format!("{}\nUser request: {prompt}", context_summary(&ctx));
        let mut contents = vec![Content::user(vec![Part::text(first)])];

        let mut cache: HashMap<String, Song> = HashMap::new();
        let mut side_effected = false;
        let mut model = self.model;

        for _round in 0..MAX_ROUNDS {
            let req = GenerateContentRequest {
                contents: contents.clone(),
                system_instruction: Some(system.clone()),
                tools: Some(vec![Tool { function_declarations: decls.clone() }]),
                generation_config: Some(gen_cfg.clone()),
            };

            let resp = match self.generate(&req, &mut model, side_effected).await {
                Ok(r) => r,
                Err(e) => {
                    let _ = self.msg_tx.send(Msg::AiError(e.to_string()));
                    return;
                }
            };

            if let Some(reason) = resp.block_reason() {
                let _ = self.msg_tx.send(Msg::AiError(format!(
                    "{} ({reason}).",
                    crate::t!("Request blocked", "요청이 차단되었어요")
                )));
                return;
            }
            let Some(content) = resp.content().cloned() else {
                let _ = self
                    .msg_tx
                    .send(Msg::AiError(crate::t!("Empty response from Gemini.", "Gemini 응답이 비어 있어요.").to_owned()));
                return;
            };

            let calls: Vec<client::FunctionCall> =
                content.function_calls().into_iter().cloned().collect();
            let text = content.joined_text();

            // No tool calls → this is the final answer.
            if calls.is_empty() {
                let mut out = text;
                if resp.finish_reason() == Some("MAX_TOKENS") {
                    out.push_str(crate::t!("\n…(response truncated)", "\n…(응답이 잘렸어요)"));
                }
                let _ = self.msg_tx.send(Msg::AiChat(out));
                return;
            }

            // Surface any interim narration, then echo the model turn verbatim (this keeps
            // `thoughtSignature`, without which the follow-up turn 400s).
            if !text.trim().is_empty() {
                let _ = self.msg_tx.send(Msg::AiChat(text));
            }
            contents.push(content);

            // Execute the tools and feed results back as a new user turn.
            let mut response_parts = Vec::with_capacity(calls.len());
            for call in &calls {
                let result = {
                    let mut deps = tools::ToolDeps {
                        ctx: &ctx,
                        cache: &mut cache,
                        msg_tx: &self.msg_tx,
                        side_effected: &mut side_effected,
                    };
                    tools::execute_tool(&call.name, &call.args, &mut deps).await
                };
                response_parts.push(Part::function_response(&call.name, result));
            }
            contents.push(Content::user(response_parts));
        }

        let _ = self.msg_tx.send(Msg::AiError(
            crate::t!(
                "Stopped after too many tool steps — try a simpler request.",
                "도구 호출이 너무 많아 중단했어요 — 좀 더 간단히 요청해 보세요."
            )
            .to_owned(),
        ));
    }

    /// One-shot ids-only radio rerank. Always emits [`Msg::RadioAiPicks`]; the ids are empty on
    /// any failure (timeout, error, block, unparseable JSON), and the reducer then degrades to
    /// the local pick. The model can never invent a track — the reducer validates every id
    /// against the shortlist this call was built from.
    async fn rerank(&mut self, seed_video_id: String, prompt: String) {
        let _guard = ThinkingGuard(self.msg_tx.clone());
        let req = build_rerank_request(&prompt);
        let ids = self.rerank_call(&req).await.unwrap_or_default();
        let _ = self.msg_tx.send(Msg::RadioAiPicks { seed_video_id, ids });
    }

    /// Run the reranker model chain (Flash-Lite → Flash), each under a hard timeout. Returns the
    /// parsed ids, or `None` ("use the local fallback") on a timeout, a transient error once the
    /// chain is exhausted, a block/early-stop finish, or unparseable JSON — none of which we
    /// retry (the local pick is already a good answer).
    async fn rerank_call(&mut self, req: &GenerateContentRequest) -> Option<Vec<String>> {
        const CHAIN: [GeminiModel; 2] = [GeminiModel::FlashLite, GeminiModel::Flash];
        for (i, &model) in CHAIN.iter().enumerate() {
            self.throttle().await;
            let resp = match timeout(RERANK_TIMEOUT, self.client.generate(model, req)).await {
                Ok(Ok(r)) => r,
                Ok(Err(e)) => {
                    if e.is_model_fallbackable() && i + 1 < CHAIN.len() {
                        tracing::warn!(from = model.label(), error = %e, "rerank model fallback");
                        continue;
                    }
                    tracing::warn!(error = %e, "rerank failed → local fallback");
                    return None;
                }
                Err(_) => {
                    tracing::warn!(timeout_s = RERANK_TIMEOUT.as_secs(), "rerank timed out → local fallback");
                    return None;
                }
            };
            if let Some(reason) = resp.block_reason() {
                tracing::warn!(reason, "rerank blocked → local fallback");
                return None;
            }
            // A truncated/safety stop yields no usable JSON — fall back rather than retry.
            if matches!(resp.finish_reason(), Some("MAX_TOKENS" | "SAFETY" | "RECITATION")) {
                tracing::warn!(finish = resp.finish_reason(), "rerank stopped early → local fallback");
                return None;
            }
            let text = resp.content().map(Content::joined_text).unwrap_or_default();
            return parse_rerank_ids(&text);
        }
        None
    }
}

/// Build the structured-output rerank request: JSON-only, thinking off, no tools. The strict
/// schema + low temperature make this a near-deterministic selection over the shortlist.
fn build_rerank_request(prompt: &str) -> GenerateContentRequest {
    let schema = serde_json::json!({
        "type": "object",
        "properties": {
            "ids": { "type": "array", "items": { "type": "string" }, "minItems": 1, "maxItems": 20 },
            "conf": { "type": "number", "minimum": 0, "maximum": 1 }
        },
        "required": ["ids"],
        "propertyOrdering": ["ids", "conf"]
    });
    GenerateContentRequest {
        contents: vec![Content::user(vec![Part::text(prompt)])],
        system_instruction: Some(Content {
            role: None,
            parts: vec![Part::text(RERANK_SYSTEM_PROMPT)],
        }),
        tools: None,
        generation_config: Some(GenerationConfig {
            temperature: Some(RERANK_TEMPERATURE),
            max_output_tokens: Some(RERANK_MAX_TOKENS),
            response_mime_type: Some("application/json".to_owned()),
            response_schema: Some(schema),
            thinking_config: Some(ThinkingConfig { thinking_budget: 0 }),
            ..Default::default()
        }),
    }
}

/// Parse `{"ids":[...]}` (tolerating a stray code fence). Returns `None` on anything unusable so
/// the caller falls back to the local pick.
fn parse_rerank_ids(text: &str) -> Option<Vec<String>> {
    let v: serde_json::Value = serde_json::from_str(strip_code_fence(text.trim())).ok()?;
    let ids: Vec<String> = v
        .get("ids")?
        .as_array()?
        .iter()
        .filter_map(|x| x.as_str().map(str::to_owned))
        .collect();
    if ids.is_empty() { None } else { Some(ids) }
}

/// Strip a leading/trailing ```` ```json ```` / ```` ``` ```` fence if the model wrapped its
/// JSON despite the JSON mime type.
fn strip_code_fence(s: &str) -> &str {
    let s = s.strip_prefix("```json").or_else(|| s.strip_prefix("```")).unwrap_or(s);
    s.strip_suffix("```").unwrap_or(s).trim()
}

/// A compact, human-readable snapshot of player state for the model's first turn.
fn context_summary(ctx: &AiContext) -> String {
    let mut s = String::from("Current player state:\n");
    s.push_str(&format!("- Now playing: {}\n", ctx.current_track.as_deref().unwrap_or("nothing")));
    if !ctx.queue_upcoming.is_empty() {
        s.push_str(&format!("- Up next: {}\n", ctx.queue_upcoming.join("; ")));
    }
    s.push_str(&format!("- Queue: {} track(s), {} remaining\n", ctx.queue_len, ctx.queue_remaining));
    if !ctx.recent_history.is_empty() {
        s.push_str(&format!("- Recently played: {}\n", ctx.recent_history.join("; ")));
    }
    if !ctx.favorites.is_empty() {
        s.push_str(&format!("- Favorites: {}\n", ctx.favorites.join("; ")));
    }
    if !ctx.playlists.is_empty() {
        let pls: Vec<String> = ctx.playlists.iter().map(|p| format!("{} ({})", p.name, p.count)).collect();
        s.push_str(&format!("- Playlists: {}\n", pls.join("; ")));
    }
    s.push_str(&format!("- Autoplay radio: {}\n", if ctx.autoplay_radio { "on" } else { "off" }));
    s.push_str(&format!("- Signed in: {}\n", if ctx.authenticated { "yes" } else { "no (anonymous)" }));
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::PlaylistInfo;

    fn ctx() -> AiContext {
        AiContext {
            current_track: Some("Song — Artist".to_owned()),
            queue_upcoming: vec!["Next — Artist".to_owned()],
            queue_len: 3,
            queue_remaining: 2,
            recent_history: vec!["Old — Artist".to_owned()],
            favorites: vec!["Fave — Artist".to_owned()],
            playlists: vec![PlaylistInfo { id: "mix".to_owned(), name: "Mix".to_owned(), count: 4 }],
            authenticated: true,
            autoplay_radio: false,
        }
    }

    #[test]
    fn context_summary_includes_key_state() {
        let s = context_summary(&ctx());
        assert!(s.contains("Now playing: Song — Artist"));
        assert!(s.contains("2 remaining"));
        assert!(s.contains("Mix (4)"));
        assert!(s.contains("Signed in: yes"));
    }

    #[test]
    fn parse_rerank_ids_reads_ids_array() {
        let ids = parse_rerank_ids(r#"{"ids":["a","b","c"],"conf":0.9}"#).unwrap();
        assert_eq!(ids, vec!["a", "b", "c"]);
    }

    #[test]
    fn parse_rerank_ids_tolerates_a_code_fence() {
        let ids = parse_rerank_ids("```json\n{\"ids\":[\"x\"]}\n```").unwrap();
        assert_eq!(ids, vec!["x"]);
    }

    #[test]
    fn parse_rerank_ids_rejects_garbage_and_empty() {
        assert!(parse_rerank_ids("not json").is_none());
        assert!(parse_rerank_ids(r#"{"ids":[]}"#).is_none(), "empty ids → fall back to local");
        assert!(parse_rerank_ids(r#"{"other":1}"#).is_none());
    }

    #[test]
    fn rerank_request_is_json_only_with_thinking_off_and_no_tools() {
        let req = build_rerank_request("seed + candidates");
        assert!(req.tools.is_none(), "reranker must not expose tools");
        let v = serde_json::to_value(&req).unwrap();
        let gc = &v["generationConfig"];
        assert_eq!(gc["responseMimeType"], "application/json");
        assert_eq!(gc["thinkingConfig"]["thinkingBudget"], 0);
        assert_eq!(gc["maxOutputTokens"], RERANK_MAX_TOKENS);
        assert!(gc["responseSchema"]["properties"].get("ids").is_some());
    }
}
