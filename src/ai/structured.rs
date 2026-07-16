//! Pure builders and parsers for Gemini structured-output tasks.

use std::time::Duration;

use crate::romanize::{RomanizeItem, RomanizedResult};

use super::client::{Content, GenerateContentRequest, GenerationConfig, Part, ThinkingConfig};
use super::dto::AiPick;

/// Reranker wall-clock budget: degrade to the local pick past this (Flash-Lite p99 can spike
/// to several seconds, and we never want a slow rerank to stall the queue top-up).
pub(super) const RERANK_TIMEOUT: Duration = Duration::from_secs(9);
/// Pure selection task → low variance.
pub(super) const RERANK_TEMPERATURE: f64 = 0.1;
/// Thinking is off, but the enriched reply (ids + roles + per-pick reason codes) needs headroom;
/// a tight cap truncates the JSON to `MAX_TOKENS` and loses the picks.
pub(super) const RERANK_MAX_TOKENS: u32 = 768;
pub(super) const RERANK_SYSTEM_PROMPT: &str = "\
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
pub(super) const FEEDBACK_TIMEOUT: Duration = Duration::from_secs(9);
/// Two short string arrays — a tight cap is plenty.
pub(super) const FEEDBACK_MAX_TOKENS: u32 = 256;
pub(super) const FEEDBACK_SYSTEM_PROMPT: &str = "\
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
pub(super) const ROMANIZE_TIMEOUT: Duration = Duration::from_secs(9);
pub(super) const ROMANIZE_MAX_TOKENS: u32 = 2048;
pub(super) const ROMANIZE_SYSTEM_PROMPT: &str = "\
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

/// Build a structured-output romanization request: JSON-only, thinking off, no tools.
pub(super) fn build_romanize_request(items: &[RomanizeItem]) -> GenerateContentRequest {
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
pub(super) fn parse_romanized_titles(
    text: &str,
    items: &[RomanizeItem],
) -> Option<Vec<RomanizedResult>> {
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
pub(super) fn build_feedback_request(digest: &str) -> GenerateContentRequest {
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
pub(super) fn parse_feedback_patch(text: &str) -> Option<(Vec<String>, Vec<String>)> {
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
pub(super) fn build_rerank_request(prompt: &str) -> GenerateContentRequest {
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
pub(super) fn parse_rerank_picks(text: &str) -> Option<(Vec<AiPick>, Option<f32>)> {
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
pub(super) fn strip_code_fence(s: &str) -> &str {
    let s = s
        .strip_prefix("```json")
        .or_else(|| s.strip_prefix("```"))
        .unwrap_or(s);
    s.strip_suffix("```").unwrap_or(s).trim()
}
