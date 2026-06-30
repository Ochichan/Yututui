//! The DJ Gem assistant: a multi-turn Gemini function-calling agent that drives playback.
//!
//! Mirrors `youtube-music-cli`'s LLM service, adapted to this app's TEA architecture: the
//! actor can't touch `App`, so tool side-effects flow back as [`crate::app::Msg`]s that
//! `update()` applies. The model invokes tools (search, play, queue, streaming, playlists);
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
pub mod usage;

pub use model::GeminiModel;

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tokio::time::{sleep, timeout};

use crate::api::Song;
use crate::app::{AiContext, AiPick, Msg};
use crate::romanize::{RomanizeItem, RomanizedResult};
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
/// Thinking is off, but the enriched reply (ids + roles + per-pick reason codes) needs headroom;
/// a tight cap truncates the JSON to `MAX_TOKENS` and loses the picks.
const RERANK_MAX_TOKENS: u32 = 768;
const RERANK_SYSTEM_PROMPT: &str = "\
You are StreamingNext, a JSON-only streaming reranker for a music player.

Input: a few header/context lines (TASK, RECIPE, POLICY, RULE, RECENT — the recent session, most recent last), \
then a CANDS block with one candidate per line:
  cid|a=artist|t=title|src=source|co=..|tr=..|u=..|nov=..|cont=..|comp=..|m=..|ver=version
The cid is an opaque id. The numbers are 0-100 evidence scores already computed for you — use \
them, do not recompute: co=co-occurrence with the recent session, tr=transition fit from the \
current track, u=the listener's affinity, nov=novelty, cont=continuation of the current source, \
comp=typical completion rate, m=official-music tier. ver is the version (song/live/remix/cover/\
acoustic/instrumental).

Rules:
- The CANDS are UNORDERED. Ignore their line position; rank purely on the evidence.
- Pick ONLY cids that appear in CANDS — never invent, alter, or merge a cid.
- Follow RECIPE as hard intent: satisfy familiar/bridge/discovery slot minimums when the \
candidate evidence allows it, and never exceed max_same_artist.
- Follow POLICY for version handling: Focused should stay canonical and familiar; Discovery may \
use live/acoustic/deep-cut performances only when they are complete music tracks.
- Shape a pleasant arc slot by slot: open with a bridge that flows from the current track \
(high tr), settle into core picks the listener will love (high co/u/m), weave in adjacent \
choices and a little discovery (higher nov) so it doesn't stagnate. Avoid the same artist \
back-to-back. Prefer official songs over live/remix/cover unless one clearly fits.
- If a RECOVERY line is present, adapt to it: after last_skip=<artist>, steer away from that \
artist and lean wider (favor higher nov); after last_like=<artist>, stay in that lane but not \
the exact same artist back-to-back; avoid any disliked=<artist> entirely; on skip_streak>=3, \
make the FIRST pick a recovery role — a safe, high-affinity (high u), high-completion (high \
comp) track to win the listener back.

Output ONLY this JSON, nothing else:
{\"ids\":[cid,...],\"roles\":[role,...],\"reasons\":[[code,...],...],\"conf\":0.0}
- ids: chosen cids best-first (up to the requested n).
- roles: parallel to ids, each one of core|bridge|adjacent|discovery|stabilizer|recovery.
- reasons: parallel to ids, each a short list of the score codes that justified it, e.g. \
[\"tr\",\"u\"].
- conf: your overall confidence in [0,1].";

/// Feedback summary is off the hot path but still shouldn't hang; reuse the reranker's budget.
const FEEDBACK_TIMEOUT: Duration = Duration::from_secs(9);
/// Two short string arrays — a tight cap is plenty.
const FEEDBACK_MAX_TOKENS: u32 = 256;
const FEEDBACK_SYSTEM_PROMPT: &str = "\
You are StationTuner, a JSON-only feedback summarizer for a music streaming station.

Input: a STATION line (the station's vibe), an optional ALREADY_AVOIDING line, and a SESSION \
log of recent outcomes (played / liked / skipped / skipped_fast / disliked), most recent last. \
Each line names an artist key.

Decide, for THIS station only:
- down_artists: artists the listener clearly dislikes here — repeatedly skipped, skipped almost \
immediately (skipped_fast), or disliked. Be conservative: ignore a single ordinary skip; require \
a real pattern.
- boost_artists: artists they clearly warmed to — liked, or consistently played to completion. \
Use this to un-avoid an artist they're now enjoying.

Never put an artist in both lists. Use the exact artist keys from the SESSION log. If the evidence \
is weak, return empty arrays.

Output ONLY this JSON: {\"down_artists\":[name,...],\"boost_artists\":[name,...]}";

/// Title romanization should stay snappy; the local fallback is already visible.
const ROMANIZE_TIMEOUT: Duration = Duration::from_secs(9);
const ROMANIZE_MAX_TOKENS: u32 = 2048;
const ROMANIZE_SYSTEM_PROMPT: &str = "\
You are MusicLatinizer, a JSON-only metadata normalizer for a terminal music player.

Task:
- Convert Korean, Japanese, Chinese, and other non-Latin song titles/artists into readable Latin
  script for display.
- Prefer romanization/transliteration by sound, not meaning translation. Example: 좋은 날 -> Joheun Nal,
  not Good Day.
- Preserve existing English, numbers, punctuation, featured artist markers, and official Latin
  artist names when obvious.
- Keep each output short and ASCII-friendly. No explanations, no extra keys.
- Use the exact input id for each item. If an item already reads well in Latin, return it unchanged.

Output ONLY this JSON:
{\"items\":[{\"id\":\"0\",\"title_latin\":\"...\",\"artist_latin\":\"...\",\"confidence\":0.0}]}";

const SYSTEM_PROMPT: &str = "\
You are the built-in music assistant for ytm-tui, a terminal YouTube Music player. You \
control real playback through the provided tools — when the user asks for music, take \
action with tools rather than only describing it. Typically search_tracks first to get \
videoIds, then play_music or add_to_queue with those ids. Keep replies short and \
friendly, and reply in the user's language. Prefer the user's own queue, favorites, and \
playlists when relevant. If a request is ambiguous, make a reasonable choice and proceed. \
When the current item is a live radio station and the user asks what song is playing, answer \
from current radio stream metadata if present; if absent, say the station has not exposed the \
current song metadata yet. Do not guess. \
Never fabricate tool results or videoIds you haven't seen.";

/// Commands sent to the Gemini actor.
pub enum AiCmd {
    Ask {
        prompt: String,
        context: Box<AiContext>,
    },
    /// One-shot streaming rerank over a local candidate pack (the autoplay path); the model picks
    /// opaque cids the reducer resolves back to tracks.
    Rerank {
        seed_video_id: String,
        prompt: String,
    },
    /// Off-path: distill a recent-feedback digest into station avoid/boost artists.
    SummarizeFeedback { digest: String },
    /// Batch Latin-script display upgrades for CJK title/artist metadata.
    Romanize {
        request_id: u64,
        items: Vec<RomanizeItem>,
    },
    /// Switch the model used for subsequent requests (settings save).
    SetModel(GeminiModel),
}

/// Handle for issuing Gemini-backed requests; results return as [`Msg`]s.
pub struct AiHandle {
    tx: UnboundedSender<AiCmd>,
}

impl AiHandle {
    pub fn ask(&self, prompt: String, context: Box<AiContext>) {
        let _ = self.tx.send(AiCmd::Ask { prompt, context });
    }

    /// Kick off a one-shot streaming rerank; the result returns as [`Msg::StreamingAiPicks`].
    pub fn rerank(&self, seed_video_id: String, prompt: String) {
        let _ = self.tx.send(AiCmd::Rerank {
            seed_video_id,
            prompt,
        });
    }

    /// Kick off an off-path feedback summary; the result returns as [`Msg::StationPatch`].
    pub fn summarize_feedback(&self, digest: String) {
        let _ = self.tx.send(AiCmd::SummarizeFeedback { digest });
    }

    /// Kick off a batch title/artist romanization upgrade.
    pub fn romanize(&self, request_id: u64, items: Vec<RomanizeItem>) {
        let _ = self.tx.send(AiCmd::Romanize { request_id, items });
    }

    /// Hot-swap the model for future requests. Ignored if the actor has stopped.
    pub fn set_model(&self, model: GeminiModel) {
        let _ = self.tx.send(AiCmd::SetModel(model));
    }
}

/// Spawn the Gemini actor. Returns `None` if the key can't form a valid header.
pub fn spawn(api_key: &str, model: GeminiModel, msg_tx: UnboundedSender<Msg>) -> Option<AiHandle> {
    let client = GeminiClient::new(api_key).ok()?;
    let (tx, rx) = mpsc::unbounded_channel();
    let actor = AiActor {
        client,
        model,
        msg_tx,
        call_times: VecDeque::new(),
    };
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
                AiCmd::Rerank {
                    seed_video_id,
                    prompt,
                } => self.rerank(seed_video_id, prompt).await,
                AiCmd::SummarizeFeedback { digest } => self.summarize_feedback(digest).await,
                AiCmd::Romanize { request_id, items } => {
                    self.romanize_titles(request_id, items).await
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
                        tracing::warn!(from = model.label(), to = fb.label(), error = %e, "DJ Gem model fallback");
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
            format!(
                "{SYSTEM_PROMPT}\n\nRespond in Korean (한국어) regardless of the language the user writes in."
            )
        } else {
            SYSTEM_PROMPT.to_owned()
        };
        let system = Content {
            role: None,
            parts: vec![Part::text(system_text)],
        };
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
                tools: Some(vec![Tool {
                    function_declarations: decls.clone(),
                }]),
                generation_config: Some(gen_cfg.clone()),
            };

            let started = Instant::now();
            let resp = match self.generate(&req, &mut model, side_effected).await {
                Ok(r) => r,
                Err(e) => {
                    let _ = self.msg_tx.send(Msg::AiError(e.to_string()));
                    return;
                }
            };
            usage::append(&usage::AiUsageRecord::new(
                "chat",
                model,
                resp.usage(),
                started.elapsed().as_millis() as u64,
                true,
                0,
                false,
            ));

            if let Some(reason) = resp.block_reason() {
                let _ = self.msg_tx.send(Msg::AiError(format!(
                    "{} ({reason}).",
                    crate::t!("Request blocked", "요청이 차단되었어요")
                )));
                return;
            }
            let Some(content) = resp.content().cloned() else {
                let _ = self.msg_tx.send(Msg::AiError(
                    crate::t!("Empty response from Gemini.", "Gemini 응답이 비어 있어요.")
                        .to_owned(),
                ));
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

    /// One-shot streaming rerank. Always emits [`Msg::StreamingAiPicks`]; the picks are empty on any
    /// failure (timeout, error, block, unparseable JSON), and the reducer then degrades to the
    /// local pick. The model can never invent a track — it picks opaque `cid`s, and the reducer
    /// resolves each one against the candidate pack this call was built from.
    async fn rerank(&mut self, seed_video_id: String, prompt: String) {
        let _guard = ThinkingGuard(self.msg_tx.clone());
        let req = build_rerank_request(&prompt);
        let (picks, conf) = self.rerank_call(&req).await.unwrap_or_default();
        let _ = self.msg_tx.send(Msg::StreamingAiPicks {
            seed_video_id,
            picks,
            conf,
        });
    }

    /// Run the reranker model chain (Flash-Lite → Flash), each under a hard timeout. Returns the
    /// parsed picks (and overall confidence), or `None` ("use the local fallback") on a timeout, a
    /// transient error once the chain is exhausted, a block/early-stop finish, or unparseable JSON
    /// — none of which we retry (the local pick is already a good answer).
    async fn rerank_call(
        &mut self,
        req: &GenerateContentRequest,
    ) -> Option<(Vec<AiPick>, Option<f32>)> {
        const CHAIN: [GeminiModel; 2] = [GeminiModel::FlashLite, GeminiModel::Flash];
        for (i, &model) in CHAIN.iter().enumerate() {
            self.throttle().await;
            let started = Instant::now();
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
                    tracing::warn!(
                        timeout_s = RERANK_TIMEOUT.as_secs(),
                        "rerank timed out → local fallback"
                    );
                    return None;
                }
            };
            let latency_ms = started.elapsed().as_millis() as u64;
            if let Some(reason) = resp.block_reason() {
                usage::append(&usage::AiUsageRecord::new(
                    "rerank",
                    model,
                    resp.usage(),
                    latency_ms,
                    false,
                    0,
                    true,
                ));
                tracing::warn!(reason, "rerank blocked → local fallback");
                return None;
            }
            // A truncated/safety stop yields no usable JSON — fall back rather than retry.
            if matches!(
                resp.finish_reason(),
                Some("MAX_TOKENS" | "SAFETY" | "RECITATION")
            ) {
                usage::append(&usage::AiUsageRecord::new(
                    "rerank",
                    model,
                    resp.usage(),
                    latency_ms,
                    false,
                    0,
                    true,
                ));
                tracing::warn!(
                    finish = resp.finish_reason(),
                    "rerank stopped early → local fallback"
                );
                return None;
            }
            let text = resp.content().map(Content::joined_text).unwrap_or_default();
            let parsed = parse_rerank_picks(&text);
            usage::append(&usage::AiUsageRecord::new(
                "rerank",
                model,
                resp.usage(),
                latency_ms,
                parsed.is_some(),
                parsed.as_ref().map_or(0, |(p, _)| p.len()),
                parsed.is_none(),
            ));
            return parsed;
        }
        None
    }

    /// One-shot, off-the-hot-path feedback summary. Turns the recent session log into a small
    /// avoid/boost patch for the active station. Always emits [`Msg::StationPatch`] (empty on any
    /// failure) so the reducer's in-flight guard always clears. No [`ThinkingGuard`]: this never
    /// runs while the user is waiting on a pick, so it must not flip the "DJ Gem is thinking" spinner.
    async fn summarize_feedback(&mut self, digest: String) {
        let req = build_feedback_request(&digest);
        let (down_artists, boost_artists) = self.feedback_call(&req).await.unwrap_or_default();
        let _ = self.msg_tx.send(Msg::StationPatch {
            down_artists,
            boost_artists,
        });
    }

    /// Run the feedback model chain (Flash-Lite → Flash) under a hard timeout. Mirrors
    /// [`Self::rerank_call`]: returns `None` (→ empty patch, a no-op) on timeout, exhausted error,
    /// block/early-stop, or unparseable JSON. None of these retry — a missed summary just means the
    /// station learns nothing this round, which is harmless.
    async fn feedback_call(
        &mut self,
        req: &GenerateContentRequest,
    ) -> Option<(Vec<String>, Vec<String>)> {
        const CHAIN: [GeminiModel; 2] = [GeminiModel::FlashLite, GeminiModel::Flash];
        for (i, &model) in CHAIN.iter().enumerate() {
            self.throttle().await;
            let started = Instant::now();
            let resp = match timeout(FEEDBACK_TIMEOUT, self.client.generate(model, req)).await {
                Ok(Ok(r)) => r,
                Ok(Err(e)) => {
                    if e.is_model_fallbackable() && i + 1 < CHAIN.len() {
                        tracing::warn!(from = model.label(), error = %e, "feedback model fallback");
                        continue;
                    }
                    tracing::warn!(error = %e, "feedback summary failed → no patch");
                    return None;
                }
                Err(_) => {
                    tracing::warn!(
                        timeout_s = FEEDBACK_TIMEOUT.as_secs(),
                        "feedback summary timed out → no patch"
                    );
                    return None;
                }
            };
            let latency_ms = started.elapsed().as_millis() as u64;
            if let Some(reason) = resp.block_reason() {
                usage::append(&usage::AiUsageRecord::new(
                    "feedback",
                    model,
                    resp.usage(),
                    latency_ms,
                    false,
                    0,
                    true,
                ));
                tracing::warn!(reason, "feedback summary blocked → no patch");
                return None;
            }
            if matches!(
                resp.finish_reason(),
                Some("MAX_TOKENS" | "SAFETY" | "RECITATION")
            ) {
                usage::append(&usage::AiUsageRecord::new(
                    "feedback",
                    model,
                    resp.usage(),
                    latency_ms,
                    false,
                    0,
                    true,
                ));
                tracing::warn!(
                    finish = resp.finish_reason(),
                    "feedback summary stopped early → no patch"
                );
                return None;
            }
            let text = resp.content().map(Content::joined_text).unwrap_or_default();
            let parsed = parse_feedback_patch(&text);
            usage::append(&usage::AiUsageRecord::new(
                "feedback",
                model,
                resp.usage(),
                latency_ms,
                parsed.is_some(),
                parsed.as_ref().map_or(0, |(d, b)| d.len() + b.len()),
                parsed.is_none(),
            ));
            return parsed;
        }
        None
    }

    /// One-shot title romanization upgrade. Always emits [`Msg::RomanizedTitles`]; empty entries
    /// mean the local fallback remains in use.
    async fn romanize_titles(&mut self, request_id: u64, items: Vec<RomanizeItem>) {
        let keys: Vec<String> = items.iter().map(|item| item.key.clone()).collect();
        let req = build_romanize_request(&items);
        let entries = self.romanize_call(&req, &items).await.unwrap_or_default();
        let _ = self.msg_tx.send(Msg::RomanizedTitles {
            request_id,
            keys,
            entries,
        });
    }

    /// Run the romanizer model chain (Flash-Lite → Flash) under a hard timeout. A failure returns
    /// `None`, leaving the already-visible local romanizer result untouched.
    async fn romanize_call(
        &mut self,
        req: &GenerateContentRequest,
        items: &[RomanizeItem],
    ) -> Option<Vec<RomanizedResult>> {
        const CHAIN: [GeminiModel; 2] = [GeminiModel::FlashLite, GeminiModel::Flash];
        for (i, &model) in CHAIN.iter().enumerate() {
            self.throttle().await;
            let started = Instant::now();
            let resp = match timeout(ROMANIZE_TIMEOUT, self.client.generate(model, req)).await {
                Ok(Ok(r)) => r,
                Ok(Err(e)) => {
                    if e.is_model_fallbackable() && i + 1 < CHAIN.len() {
                        tracing::warn!(from = model.label(), error = %e, "romanize model fallback");
                        continue;
                    }
                    tracing::warn!(error = %e, "romanize failed → local fallback");
                    return None;
                }
                Err(_) => {
                    tracing::warn!(
                        timeout_s = ROMANIZE_TIMEOUT.as_secs(),
                        "romanize timed out → local fallback"
                    );
                    return None;
                }
            };
            let latency_ms = started.elapsed().as_millis() as u64;
            if let Some(reason) = resp.block_reason() {
                usage::append(&usage::AiUsageRecord::new(
                    "romanize",
                    model,
                    resp.usage(),
                    latency_ms,
                    false,
                    0,
                    true,
                ));
                tracing::warn!(reason, "romanize blocked → local fallback");
                return None;
            }
            if matches!(
                resp.finish_reason(),
                Some("MAX_TOKENS" | "SAFETY" | "RECITATION")
            ) {
                usage::append(&usage::AiUsageRecord::new(
                    "romanize",
                    model,
                    resp.usage(),
                    latency_ms,
                    false,
                    0,
                    true,
                ));
                tracing::warn!(finish = resp.finish_reason(), "romanize stopped early");
                return None;
            }
            let text = resp.content().map(Content::joined_text).unwrap_or_default();
            let parsed = parse_romanized_titles(&text, items);
            usage::append(&usage::AiUsageRecord::new(
                "romanize",
                model,
                resp.usage(),
                latency_ms,
                parsed.is_some(),
                parsed.as_ref().map_or(0, Vec::len),
                parsed.is_none(),
            ));
            return parsed;
        }
        None
    }
}

/// Build a structured-output romanization request: JSON-only, thinking off, no tools.
fn build_romanize_request(items: &[RomanizeItem]) -> GenerateContentRequest {
    let input_items: Vec<serde_json::Value> = items
        .iter()
        .enumerate()
        .map(|(i, item)| {
            serde_json::json!({
                "id": i.to_string(),
                "title": item.title.as_str(),
                "artist": item.artist.as_str(),
            })
        })
        .collect();
    let prompt = serde_json::to_string(&serde_json::json!({ "items": input_items }))
        .unwrap_or_else(|_| "{\"items\":[]}".to_owned());
    let schema = serde_json::json!({
        "type": "object",
        "properties": {
            "items": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "id": { "type": "string" },
                        "title_latin": { "type": "string" },
                        "artist_latin": { "type": "string" },
                        "confidence": { "type": "number", "minimum": 0, "maximum": 1 }
                    },
                    "required": ["id", "title_latin", "artist_latin"],
                    "propertyOrdering": ["id", "title_latin", "artist_latin", "confidence"]
                }
            }
        },
        "required": ["items"],
        "propertyOrdering": ["items"]
    });
    GenerateContentRequest {
        contents: vec![Content::user(vec![Part::text(prompt)])],
        system_instruction: Some(Content {
            role: None,
            parts: vec![Part::text(ROMANIZE_SYSTEM_PROMPT)],
        }),
        tools: None,
        generation_config: Some(GenerationConfig {
            temperature: Some(RERANK_TEMPERATURE),
            max_output_tokens: Some(ROMANIZE_MAX_TOKENS),
            response_mime_type: Some("application/json".to_owned()),
            response_schema: Some(schema),
            thinking_config: Some(ThinkingConfig { thinking_budget: 0 }),
            ..Default::default()
        }),
    }
}

/// Parse `{"items":[{"id":"0","title_latin":"...","artist_latin":"...","confidence":0.9}]}`.
fn parse_romanized_titles(text: &str, items: &[RomanizeItem]) -> Option<Vec<RomanizedResult>> {
    let v: serde_json::Value = serde_json::from_str(strip_code_fence(text.trim())).ok()?;
    let arr = v.get("items")?.as_array()?;
    let mut out = Vec::new();
    for item in arr {
        let id = item.get("id")?.as_str()?;
        let idx = id.parse::<usize>().ok()?;
        let original = items.get(idx)?;
        let title = item
            .get("title_latin")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_owned();
        let artist = item
            .get("artist_latin")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_owned();
        if title.is_empty() && artist.is_empty() {
            continue;
        }
        let confidence = item
            .get("confidence")
            .and_then(serde_json::Value::as_f64)
            .map(|c| (c as f32).clamp(0.0, 1.0));
        out.push(RomanizedResult {
            key: original.key.clone(),
            title,
            artist,
            confidence,
        });
    }
    Some(out)
}

/// Build the structured-output feedback request: JSON-only, thinking off, no tools. Two short
/// string arrays out — a tight token cap and low temperature keep it cheap and stable.
fn build_feedback_request(digest: &str) -> GenerateContentRequest {
    let schema = serde_json::json!({
        "type": "object",
        "properties": {
            "down_artists": { "type": "array", "items": { "type": "string" } },
            "boost_artists": { "type": "array", "items": { "type": "string" } }
        },
        "required": ["down_artists", "boost_artists"],
        "propertyOrdering": ["down_artists", "boost_artists"]
    });
    GenerateContentRequest {
        contents: vec![Content::user(vec![Part::text(digest)])],
        system_instruction: Some(Content {
            role: None,
            parts: vec![Part::text(FEEDBACK_SYSTEM_PROMPT)],
        }),
        tools: None,
        generation_config: Some(GenerationConfig {
            temperature: Some(RERANK_TEMPERATURE),
            max_output_tokens: Some(FEEDBACK_MAX_TOKENS),
            response_mime_type: Some("application/json".to_owned()),
            response_schema: Some(schema),
            thinking_config: Some(ThinkingConfig { thinking_budget: 0 }),
            ..Default::default()
        }),
    }
}

/// Parse the feedback reply `{"down_artists":[...],"boost_artists":[...]}` (tolerating a stray
/// code fence). Either array may be missing/empty. Returns `None` only when the whole payload is
/// unparseable; an empty-but-valid object yields two empty vecs (a valid no-op patch).
fn parse_feedback_patch(text: &str) -> Option<(Vec<String>, Vec<String>)> {
    let v: serde_json::Value = serde_json::from_str(strip_code_fence(text.trim())).ok()?;
    if !v.is_object() {
        return None;
    }
    let names = |key: &str| -> Vec<String> {
        v.get(key)
            .and_then(serde_json::Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(|x| {
                        x.as_str()
                            .map(str::trim)
                            .filter(|s| !s.is_empty())
                            .map(str::to_owned)
                    })
                    .collect()
            })
            .unwrap_or_default()
    };
    Some((names("down_artists"), names("boost_artists")))
}

/// Build the structured-output rerank request: JSON-only, thinking off, no tools. The strict
/// schema + low temperature make this a near-deterministic selection over the shortlist.
fn build_rerank_request(prompt: &str) -> GenerateContentRequest {
    let schema = serde_json::json!({
        "type": "object",
        "properties": {
            "ids": { "type": "array", "items": { "type": "string" }, "minItems": 1, "maxItems": 20 },
            "roles": { "type": "array", "items": {
                "type": "string",
                "enum": ["core", "bridge", "adjacent", "discovery", "stabilizer", "recovery"]
            } },
            "reasons": { "type": "array", "items": {
                "type": "array", "items": { "type": "string" }
            } },
            "conf": { "type": "number", "minimum": 0, "maximum": 1 }
        },
        "required": ["ids"],
        "propertyOrdering": ["ids", "roles", "reasons", "conf"]
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

/// Parse the enriched rerank reply
/// `{"ids":[cid,...],"roles":[...],"reasons":[[code,...],...],"conf":n}` (tolerating a stray code
/// fence). `ids` is required and non-empty; `roles`/`reasons`/`conf` are optional and zipped
/// positionally onto the ids (so an ids-only reply still parses). Returns `None` on anything
/// unusable so the caller falls back to the local pick.
fn parse_rerank_picks(text: &str) -> Option<(Vec<AiPick>, Option<f32>)> {
    let v: serde_json::Value = serde_json::from_str(strip_code_fence(text.trim())).ok()?;
    let ids: Vec<String> = v
        .get("ids")?
        .as_array()?
        .iter()
        .filter_map(|x| x.as_str().map(str::to_owned))
        .collect();
    if ids.is_empty() {
        return None;
    }
    let roles = v.get("roles").and_then(serde_json::Value::as_array);
    let reasons = v.get("reasons").and_then(serde_json::Value::as_array);
    let conf = v
        .get("conf")
        .and_then(serde_json::Value::as_f64)
        .map(|c| c as f32);

    let picks = ids
        .into_iter()
        .enumerate()
        .map(|(i, cid)| {
            let role = roles
                .and_then(|r| r.get(i))
                .and_then(|x| x.as_str())
                .map(str::to_owned);
            let reasons = reasons
                .and_then(|r| r.get(i))
                .and_then(serde_json::Value::as_array)
                .map(|arr| {
                    arr.iter()
                        .filter_map(|s| s.as_str().map(str::to_owned))
                        .collect()
                })
                .unwrap_or_default();
            AiPick { cid, role, reasons }
        })
        .collect();
    Some((picks, conf))
}

/// Strip a leading/trailing ```` ```json ```` / ```` ``` ```` fence if the model wrapped its
/// JSON despite the JSON mime type.
fn strip_code_fence(s: &str) -> &str {
    let s = s
        .strip_prefix("```json")
        .or_else(|| s.strip_prefix("```"))
        .unwrap_or(s);
    s.strip_suffix("```").unwrap_or(s).trim()
}

/// A compact, human-readable snapshot of player state for the model's first turn.
fn context_summary(ctx: &AiContext) -> String {
    let mut s = String::from("Current player state:\n");
    s.push_str(&format!(
        "- Now playing: {}\n",
        ctx.current_track.as_deref().unwrap_or("nothing")
    ));
    if let Some(station) = &ctx.current_radio_station {
        s.push_str(&format!("- Current radio station: {station}\n"));
        match &ctx.current_radio_now_playing {
            Some(track) => s.push_str(&format!("- Current radio stream track: {track}\n")),
            None => s.push_str(
                "- Current radio stream track: unavailable; this station has not exposed now-playing metadata yet\n",
            ),
        }
    }
    if !ctx.queue_upcoming.is_empty() {
        s.push_str(&format!("- Up next: {}\n", ctx.queue_upcoming.join("; ")));
    }
    s.push_str(&format!(
        "- Queue: {} track(s), {} remaining\n",
        ctx.queue_len, ctx.queue_remaining
    ));
    if !ctx.recent_history.is_empty() {
        s.push_str(&format!(
            "- Recently played: {}\n",
            ctx.recent_history.join("; ")
        ));
    }
    if !ctx.favorites.is_empty() {
        s.push_str(&format!("- Favorites: {}\n", ctx.favorites.join("; ")));
    }
    if !ctx.playlists.is_empty() {
        let pls: Vec<String> = ctx
            .playlists
            .iter()
            .map(|p| format!("{} ({})", p.name, p.count))
            .collect();
        s.push_str(&format!("- Playlists: {}\n", pls.join("; ")));
    }
    s.push_str(&format!(
        "- Autoplay streaming: {}\n",
        if ctx.autoplay_streaming { "on" } else { "off" }
    ));
    s.push_str(&format!(
        "- Signed in: {}\n",
        if ctx.authenticated {
            "yes"
        } else {
            "no (anonymous)"
        }
    ));
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::PlaylistInfo;

    fn ctx() -> AiContext {
        AiContext {
            current_track: Some("Song — Artist".to_owned()),
            current_radio_station: None,
            current_radio_now_playing: None,
            queue_upcoming: vec!["Next — Artist".to_owned()],
            queue_len: 3,
            queue_remaining: 2,
            recent_history: vec!["Old — Artist".to_owned()],
            favorites: vec!["Fave — Artist".to_owned()],
            playlists: vec![PlaylistInfo {
                id: "mix".to_owned(),
                name: "Mix".to_owned(),
                count: 4,
            }],
            authenticated: true,
            autoplay_streaming: false,
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
    fn context_summary_includes_radio_stream_metadata() {
        let mut ctx = ctx();
        ctx.current_track = Some("Groove Radio — US / MP3 / 128k".to_owned());
        ctx.current_radio_station = ctx.current_track.clone();
        ctx.current_radio_now_playing = Some("The Track — The Artist".to_owned());

        let s = context_summary(&ctx);

        assert!(s.contains("Current radio station: Groove Radio"));
        assert!(s.contains("Current radio stream track: The Track — The Artist"));
    }

    #[test]
    fn context_summary_warns_when_radio_stream_metadata_is_absent() {
        let mut ctx = ctx();
        ctx.current_track = Some("Groove Radio — US / MP3 / 128k".to_owned());
        ctx.current_radio_station = ctx.current_track.clone();

        let s = context_summary(&ctx);

        assert!(s.contains("Current radio stream track: unavailable"));
    }

    #[test]
    fn parse_rerank_picks_reads_ids_and_conf() {
        let (picks, conf) = parse_rerank_picks(r#"{"ids":["a","b","c"],"conf":0.9}"#).unwrap();
        let cids: Vec<&str> = picks.iter().map(|p| p.cid.as_str()).collect();
        assert_eq!(cids, vec!["a", "b", "c"]);
        assert_eq!(conf, Some(0.9));
        // roles/reasons absent → defaulted, not an error.
        assert!(
            picks
                .iter()
                .all(|p| p.role.is_none() && p.reasons.is_empty())
        );
    }

    #[test]
    fn parse_rerank_picks_zips_roles_and_reasons_onto_ids() {
        let (picks, _) = parse_rerank_picks(
            r#"{"ids":["a","b"],"roles":["bridge","core"],"reasons":[["tr"],["co","u"]],"conf":0.7}"#,
        )
        .unwrap();
        assert_eq!(picks[0].role.as_deref(), Some("bridge"));
        assert_eq!(picks[0].reasons, vec!["tr"]);
        assert_eq!(picks[1].role.as_deref(), Some("core"));
        assert_eq!(picks[1].reasons, vec!["co", "u"]);
    }

    #[test]
    fn parse_rerank_picks_tolerates_a_code_fence() {
        let (picks, _) = parse_rerank_picks("```json\n{\"ids\":[\"x\"]}\n```").unwrap();
        assert_eq!(picks[0].cid, "x");
    }

    #[test]
    fn parse_rerank_picks_rejects_garbage_and_empty() {
        assert!(parse_rerank_picks("not json").is_none());
        assert!(
            parse_rerank_picks(r#"{"ids":[]}"#).is_none(),
            "empty ids → fall back to local"
        );
        assert!(parse_rerank_picks(r#"{"other":1}"#).is_none());
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
        let props = &gc["responseSchema"]["properties"];
        assert!(props.get("ids").is_some());
        assert!(props.get("roles").is_some(), "schema must expose roles");
        assert!(props.get("reasons").is_some(), "schema must expose reasons");
    }

    #[test]
    fn parse_feedback_patch_reads_both_arrays_and_trims_blanks() {
        let (down, boost) = parse_feedback_patch(
            r#"{"down_artists":["Nickelback"," "],"boost_artists":["  ABBA "]}"#,
        )
        .unwrap();
        assert_eq!(down, vec!["Nickelback"]);
        assert_eq!(boost, vec!["ABBA"], "names are trimmed and blanks dropped");
    }

    #[test]
    fn parse_feedback_patch_allows_a_valid_empty_object_as_a_noop() {
        // A well-formed object with no/empty arrays is a valid no-op patch, not a parse failure.
        assert_eq!(parse_feedback_patch("{}"), Some((vec![], vec![])));
        let (down, boost) = parse_feedback_patch(r#"{"down_artists":[]}"#).unwrap();
        assert!(down.is_empty() && boost.is_empty());
    }

    #[test]
    fn parse_feedback_patch_tolerates_a_code_fence_and_rejects_garbage() {
        let (down, _) = parse_feedback_patch("```json\n{\"down_artists\":[\"X\"]}\n```").unwrap();
        assert_eq!(down, vec!["X"]);
        assert!(parse_feedback_patch("not json").is_none());
        assert!(
            parse_feedback_patch("[1,2,3]").is_none(),
            "a non-object is unusable"
        );
    }

    #[test]
    fn feedback_request_is_json_only_with_thinking_off_and_no_tools() {
        let req = build_feedback_request("STATION|...\nSESSION|...");
        assert!(
            req.tools.is_none(),
            "feedback summary must not expose tools"
        );
        let v = serde_json::to_value(&req).unwrap();
        let gc = &v["generationConfig"];
        assert_eq!(gc["responseMimeType"], "application/json");
        assert_eq!(gc["thinkingConfig"]["thinkingBudget"], 0);
        assert_eq!(gc["maxOutputTokens"], FEEDBACK_MAX_TOKENS);
        let props = &gc["responseSchema"]["properties"];
        assert!(props.get("down_artists").is_some());
        assert!(props.get("boost_artists").is_some());
    }
}
