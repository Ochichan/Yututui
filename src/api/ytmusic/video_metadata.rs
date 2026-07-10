use anyhow::Result;

use crate::streaming::{self, StreamingConfig, StreamingMode};

use super::{
    STREAMING_PREFLIGHT_TIMEOUT, YTDLP_JSON_MAX, YtMusicApi, json_string, ytmusic_ytdlp_command,
};

#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub(crate) struct YtdlpAudioSummary {
    #[serde(default)]
    pub selected_format_id: Option<String>,
    #[serde(default)]
    pub selected_format_note: Option<String>,
    #[serde(default)]
    pub selected_extension: Option<String>,
    #[serde(default)]
    pub selected_audio_codec: Option<String>,
    #[serde(default)]
    pub selected_audio_bitrate_kbps: Option<f64>,
    #[serde(default)]
    pub selected_sample_rate_hz: Option<u32>,
    #[serde(default)]
    pub available_audio_formats: Option<u32>,
    #[serde(default)]
    pub available_audio_only_formats: Option<u32>,
    #[serde(default)]
    pub max_audio_bitrate_kbps: Option<f64>,
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub(crate) struct YtdlpVideoMeta {
    pub title: String,
    pub channel: String,
    #[serde(default)]
    pub channel_id: Option<String>,
    #[serde(default)]
    pub uploader_id: Option<String>,
    /// A yt-dlp/YouTube verification signal only. This is not proof that the channel is
    /// an Official Artist Channel and must never be treated as such by ranking policy.
    #[serde(default)]
    pub channel_is_verified: Option<bool>,
    #[serde(default)]
    pub availability: Option<String>,
    #[serde(default)]
    pub extractor: Option<String>,
    #[serde(default)]
    pub extractor_key: Option<String>,
    #[serde(default)]
    pub audio: YtdlpAudioSummary,
    pub duration_secs: Option<u32>,
    pub live_status: Option<String>,
    pub is_live: Option<bool>,
    pub was_live: Option<bool>,
    pub media_type: Option<String>,
    pub description: Option<String>,
}

pub(super) async fn enrich_video_meta(video_id: &str) -> Result<YtdlpVideoMeta> {
    let url = format!("https://www.youtube.com/watch?v={video_id}");
    let mut cmd = ytmusic_ytdlp_command();
    cmd.arg("--dump-single-json")
        .arg("--no-playlist")
        .arg("--no-warnings")
        .arg(&url);
    let json = crate::tools::run_ytdlp_json(
        cmd,
        STREAMING_PREFLIGHT_TIMEOUT,
        YTDLP_JSON_MAX,
        "metadata lookup",
    )
    .await?;
    Ok(parse_ytdlp_video_meta(&json))
}

pub(super) fn parse_ytdlp_video_meta(json: &serde_json::Value) -> YtdlpVideoMeta {
    YtdlpVideoMeta {
        title: json_string(json, &["title"]).unwrap_or_default(),
        channel: json_string(json, &["channel", "uploader"]).unwrap_or_default(),
        channel_id: json_string(json, &["channel_id"]),
        uploader_id: json_string(json, &["uploader_id"]),
        channel_is_verified: json_bool(json, &["channel_is_verified"]),
        availability: json_string(json, &["availability"]),
        extractor: json_string(json, &["extractor"]),
        extractor_key: json_string(json, &["extractor_key"]),
        audio: parse_ytdlp_audio_summary(json),
        duration_secs: json
            .get("duration")
            .and_then(serde_json::Value::as_f64)
            .filter(|d| d.is_finite() && *d >= 0.0)
            .map(|d| d.round() as u32),
        live_status: json_string(json, &["live_status"]),
        is_live: json_bool(json, &["is_live"]),
        was_live: json_bool(json, &["was_live"]),
        media_type: json_string(json, &["media_type"]),
        description: json_string(json, &["description"]),
    }
}

fn parse_ytdlp_audio_summary(json: &serde_json::Value) -> YtdlpAudioSummary {
    let selected = json
        .get("requested_formats")
        .and_then(serde_json::Value::as_array)
        .and_then(|formats| formats.iter().find(|format| has_audio_codec(format)))
        .or_else(|| has_audio_codec(json).then_some(json));

    let formats = json
        .get("formats")
        .and_then(serde_json::Value::as_array)
        .map(Vec::as_slice);
    let available_audio_formats = formats.map(|formats| {
        u32::try_from(
            formats
                .iter()
                .filter(|format| has_audio_codec(format))
                .count(),
        )
        .unwrap_or(u32::MAX)
    });
    let available_audio_only_formats = formats.map(|formats| {
        u32::try_from(
            formats
                .iter()
                .filter(|format| {
                    has_audio_codec(format)
                        && json_string(format, &["vcodec"]).as_deref() == Some("none")
                })
                .count(),
        )
        .unwrap_or(u32::MAX)
    });
    let max_audio_bitrate_kbps = formats.and_then(|formats| {
        formats
            .iter()
            .filter(|format| has_audio_codec(format))
            .filter_map(audio_bitrate_kbps)
            .max_by(f64::total_cmp)
    });

    YtdlpAudioSummary {
        selected_format_id: selected.and_then(|value| json_string(value, &["format_id"])),
        selected_format_note: selected.and_then(|value| json_string(value, &["format_note"])),
        selected_extension: selected.and_then(|value| {
            json_string(value, &["audio_ext", "ext"]).filter(|ext| ext != "none")
        }),
        selected_audio_codec: selected
            .and_then(|value| json_string(value, &["acodec"]))
            .filter(|codec| !codec.eq_ignore_ascii_case("none")),
        selected_audio_bitrate_kbps: selected.and_then(audio_bitrate_kbps),
        selected_sample_rate_hz: selected.and_then(|value| json_u32(value, &["asr"])),
        available_audio_formats,
        available_audio_only_formats,
        max_audio_bitrate_kbps,
    }
}

fn has_audio_codec(json: &serde_json::Value) -> bool {
    json_string(json, &["acodec"])
        .is_some_and(|codec| !codec.trim().is_empty() && !codec.eq_ignore_ascii_case("none"))
}

fn audio_bitrate_kbps(json: &serde_json::Value) -> Option<f64> {
    json_f64(json, &["abr"]).or_else(|| {
        if json_string(json, &["vcodec"]).as_deref() == Some("none") {
            json_f64(json, &["tbr"])
        } else {
            None
        }
    })
}

pub(super) fn json_bool(json: &serde_json::Value, keys: &[&str]) -> Option<bool> {
    keys.iter()
        .find_map(|key| json.get(key).and_then(serde_json::Value::as_bool))
}

fn json_f64(json: &serde_json::Value, keys: &[&str]) -> Option<f64> {
    keys.iter().find_map(|key| {
        let value = json.get(key)?;
        value
            .as_f64()
            .or_else(|| value.as_str()?.parse::<f64>().ok())
            .filter(|number| number.is_finite() && *number >= 0.0)
    })
}

fn json_u32(json: &serde_json::Value, keys: &[&str]) -> Option<u32> {
    json_f64(json, keys).and_then(|number| {
        (number <= f64::from(u32::MAX) && number.fract() == 0.0).then_some(number as u32)
    })
}

pub(super) fn reject_enriched(
    meta: &YtdlpVideoMeta,
    mode: StreamingMode,
    cfg: &StreamingConfig,
) -> bool {
    if meta.is_live == Some(true) {
        return true;
    }
    if matches!(
        meta.live_status.as_deref(),
        Some("is_live" | "is_upcoming" | "post_live")
    ) {
        return true;
    }
    if matches!(meta.media_type.as_deref(), Some("playlist" | "multi_video")) {
        return true;
    }
    if let Some(duration) = meta.duration_secs {
        let mode_max = match mode {
            StreamingMode::Focused => 8 * 60,
            StreamingMode::Balanced => 12 * 60,
            StreamingMode::Discovery => 15 * 60,
        };
        let max_duration = cfg.max_duration_secs.min(mode_max);
        if duration < cfg.min_duration_secs || duration > max_duration {
            return true;
        }
    }
    let rich_title = match meta.description.as_deref() {
        Some(desc) if !desc.trim().is_empty() => format!("{} {}", meta.title, desc),
        _ => meta.title.clone(),
    };
    let decision = streaming::musicgate::decide(
        &rich_title,
        &meta.channel,
        streaming::CandidateSource::YtdlpStreaming,
        mode,
    );
    if decision.action == streaming::musicgate::GateAction::Reject {
        return true;
    }
    let risk = streaming::musicgate::non_music_risk_score(&rich_title, &meta.channel);
    let music_tier = streaming::musicgate::music_tier_score(&meta.title, &meta.channel);
    if mode == StreamingMode::Focused && decision.action == streaming::musicgate::GateAction::Demote
    {
        return true;
    }
    risk >= 0.70 && music_tier <= 0.0 && meta.was_live != Some(true)
}

impl YtMusicApi {
    pub(crate) async fn youtube_video_metadata(&self, video_id: &str) -> Result<YtdlpVideoMeta> {
        enrich_video_meta(video_id).await
    }
}
