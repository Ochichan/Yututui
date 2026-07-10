use std::borrow::Cow;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use ytmapi_rs::auth::AuthToken;
use ytmapi_rs::parse::{ParseFrom, ProcessedResult};
use ytmapi_rs::query::{PostMethod, PostQuery, Query};

use crate::api::Song;

/// YouTube Music's internal presentation type for a video search result.
///
/// These values are useful provenance, not an independently documented public guarantee.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum YtmMusicVideoType {
    Omv,
    OfficialSourceMusic,
    Ugc,
    Atv,
    Shoulder,
    Episode,
    Upload,
    #[default]
    Unknown,
}

impl YtmMusicVideoType {
    fn from_raw(raw: Option<&str>) -> Self {
        match raw {
            Some("MUSIC_VIDEO_TYPE_OMV") => Self::Omv,
            Some("MUSIC_VIDEO_TYPE_OFFICIAL_SOURCE_MUSIC") => Self::OfficialSourceMusic,
            Some("MUSIC_VIDEO_TYPE_UGC") => Self::Ugc,
            Some("MUSIC_VIDEO_TYPE_ATV") => Self::Atv,
            Some("MUSIC_VIDEO_TYPE_SHOULDER") => Self::Shoulder,
            Some("MUSIC_VIDEO_TYPE_PODCAST_EPISODE") => Self::Episode,
            Some("MUSIC_VIDEO_TYPE_PRIVATELY_OWNED_TRACK") => Self::Upload,
            _ => Self::Unknown,
        }
    }

    pub fn code(self) -> &'static str {
        match self {
            Self::Omv => "omv",
            Self::OfficialSourceMusic => "official_source_music",
            Self::Ugc => "ugc",
            Self::Atv => "atv",
            Self::Shoulder => "shoulder",
            Self::Episode => "episode",
            Self::Upload => "upload",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Clone)]
pub struct TransferVideoSearchResult {
    pub song: Song,
    pub music_video_type: YtmMusicVideoType,
}

/// A repository-local VideosFilter query. ytmapi-rs's public video result drops
/// `musicVideoType`, so transfer matching parses the same response without changing the crate.
#[derive(Debug)]
pub(super) struct OfficialVideoSearchQuery<'a> {
    query: Cow<'a, str>,
}

impl<'a> OfficialVideoSearchQuery<'a> {
    pub(super) fn new(query: impl Into<Cow<'a, str>>) -> Self {
        Self {
            query: query.into(),
        }
    }
}

impl<A: AuthToken> Query<A> for OfficialVideoSearchQuery<'_> {
    type Output = OfficialVideoSearchResults;
    type Method = PostMethod;
}

impl PostQuery for OfficialVideoSearchQuery<'_> {
    fn header(&self) -> Map<String, Value> {
        Map::from_iter([
            ("query".to_owned(), Value::String(self.query.to_string())),
            // VideosFilter + exact spelling, matching ytmapi-rs 0.3.2.
            (
                "params".to_owned(),
                Value::String("EgWKAQIQAWoMEA4QChADEAQQCRAF".to_owned()),
            ),
        ])
    }

    fn params(&self) -> Vec<(&str, Cow<'_, str>)> {
        Vec::new()
    }

    fn path(&self) -> &str {
        "search"
    }
}

#[derive(Debug)]
pub(super) struct OfficialVideoSearchResults(pub Vec<TransferVideoSearchResult>);

impl ParseFrom<OfficialVideoSearchQuery<'_>> for OfficialVideoSearchResults {
    fn parse_from(
        processed: ProcessedResult<OfficialVideoSearchQuery<'_>>,
    ) -> ytmapi_rs::Result<Self> {
        let value: Value = ytmapi_rs::json::from_json(processed.json)?;
        Ok(Self(parse_video_search_value(&value)))
    }
}

fn parse_video_search_value(value: &Value) -> Vec<TransferVideoSearchResult> {
    let mut renderers = Vec::new();
    collect_renderers(value, &mut renderers);
    renderers
        .into_iter()
        .filter_map(parse_renderer)
        .take(super::transfer_api::TRANSFER_YTM_RESULT_LIMIT)
        .collect()
}

fn collect_renderers<'a>(value: &'a Value, out: &mut Vec<&'a Value>) {
    match value {
        Value::Object(object) => {
            if let Some(renderer) = object.get("musicResponsiveListItemRenderer") {
                out.push(renderer);
                return;
            }
            for child in object.values() {
                collect_renderers(child, out);
            }
        }
        Value::Array(values) => {
            for child in values {
                collect_renderers(child, out);
            }
        }
        _ => {}
    }
}

fn parse_renderer(renderer: &Value) -> Option<TransferVideoSearchResult> {
    if find_string(renderer, "displayPolicy") == Some("MUSIC_ITEM_RENDERER_DISPLAY_POLICY_GREY_OUT")
    {
        return None;
    }

    let video_id = renderer
        .pointer("/playlistItemData/videoId")
        .and_then(Value::as_str)
        .or_else(|| find_string(renderer, "videoId"))?
        .trim();
    if video_id.is_empty() {
        return None;
    }

    let flex_columns = renderer.get("flexColumns")?.as_array()?;
    let title = flex_columns
        .first()
        .map(text_runs)
        .and_then(|runs| runs.into_iter().find(|run| !run.trim().is_empty()))?;
    let fields = flex_columns
        .get(1)
        .map(text_runs)
        .unwrap_or_default()
        .into_iter()
        .filter(|field| !is_separator(field))
        .collect::<Vec<_>>();

    let raw_type = find_string(renderer, "musicVideoType");
    let mut music_video_type = YtmMusicVideoType::from_raw(raw_type);
    if music_video_type == YtmMusicVideoType::Unknown
        && fields.first().is_some_and(|field| field == "Episode")
    {
        music_video_type = YtmMusicVideoType::Episode;
    }

    let channel = match fields.first().map(String::as_str) {
        Some("Video" | "Episode") => fields.get(1),
        _ => fields.first(),
    }
    .cloned()
    .unwrap_or_default();
    let duration = fields
        .iter()
        .rev()
        .find(|field| looks_like_duration(field))
        .cloned()
        .unwrap_or_default();

    Some(TransferVideoSearchResult {
        song: Song::from_search(video_id, title, channel, duration, None),
        music_video_type,
    })
}

fn text_runs(column: &Value) -> Vec<String> {
    let Some(renderer) = column.get("musicResponsiveListItemFlexColumnRenderer") else {
        return Vec::new();
    };
    renderer
        .pointer("/text/runs")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|run| run.get("text").and_then(Value::as_str))
        .map(str::to_owned)
        .collect()
}

fn find_string<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    match value {
        Value::Object(object) => {
            if let Some(value) = object.get(key).and_then(Value::as_str) {
                return Some(value);
            }
            object.values().find_map(|child| find_string(child, key))
        }
        Value::Array(values) => values.iter().find_map(|child| find_string(child, key)),
        _ => None,
    }
}

fn is_separator(value: &str) -> bool {
    value.trim().is_empty() || matches!(value.trim(), "•" | "·")
}

fn looks_like_duration(value: &str) -> bool {
    let mut parts = value.trim().split(':');
    let count = parts.clone().count();
    (count == 2 || count == 3)
        && parts.all(|part| !part.is_empty() && part.chars().all(|ch| ch.is_ascii_digit()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ytmapi_rs::query::PostQuery;

    fn renderer(video_type: Option<&str>, label: &str, video_id: &str) -> Value {
        let watch_config = video_type.map_or_else(
            || serde_json::json!({}),
            |video_type| {
                serde_json::json!({
                    "watchEndpointMusicSupportedConfigs": {
                        "watchEndpointMusicConfig": { "musicVideoType": video_type }
                    }
                })
            },
        );
        serde_json::json!({
            "musicResponsiveListItemRenderer": {
                "flexColumns": [
                    {"musicResponsiveListItemFlexColumnRenderer": {"text": {"runs": [
                        {"text": "Song (Official Music Video)", "navigationEndpoint": {
                            "watchEndpoint": watch_config
                        }}
                    ]}}},
                    {"musicResponsiveListItemFlexColumnRenderer": {"text": {"runs": [
                        {"text": label}, {"text": " • "}, {"text": "ArtistVEVO"},
                        {"text": " • "}, {"text": "1M views"}, {"text": " • "},
                        {"text": "3:04"}
                    ]}}}
                ],
                "playlistItemData": {"videoId": video_id}
            }
        })
    }

    #[test]
    fn query_matches_ytmapi_videos_filter_parameters() {
        let query = OfficialVideoSearchQuery::new("Artist Song");
        assert_eq!(query.path(), "search");
        assert_eq!(
            query.header().get("params").and_then(Value::as_str),
            Some("EgWKAQIQAWoMEA4QChADEAQQCRAF")
        );
    }

    #[test]
    fn parser_preserves_all_known_video_types_and_unknown() {
        let cases = [
            (Some("MUSIC_VIDEO_TYPE_OMV"), YtmMusicVideoType::Omv),
            (
                Some("MUSIC_VIDEO_TYPE_OFFICIAL_SOURCE_MUSIC"),
                YtmMusicVideoType::OfficialSourceMusic,
            ),
            (Some("MUSIC_VIDEO_TYPE_UGC"), YtmMusicVideoType::Ugc),
            (Some("MUSIC_VIDEO_TYPE_ATV"), YtmMusicVideoType::Atv),
            (
                Some("MUSIC_VIDEO_TYPE_SHOULDER"),
                YtmMusicVideoType::Shoulder,
            ),
            (
                Some("MUSIC_VIDEO_TYPE_PODCAST_EPISODE"),
                YtmMusicVideoType::Episode,
            ),
            (
                Some("MUSIC_VIDEO_TYPE_PRIVATELY_OWNED_TRACK"),
                YtmMusicVideoType::Upload,
            ),
            (Some("MUSIC_VIDEO_TYPE_FUTURE"), YtmMusicVideoType::Unknown),
            (None, YtmMusicVideoType::Unknown),
        ];

        let response = Value::Array(
            cases
                .iter()
                .enumerate()
                .map(|(index, (raw, _))| renderer(*raw, "Video", &format!("video-{index}")))
                .collect(),
        );
        let parsed = parse_video_search_value(&response);
        assert_eq!(parsed.len(), cases.len());
        assert_eq!(
            parsed
                .iter()
                .map(|result| result.music_video_type)
                .collect::<Vec<_>>(),
            cases
                .iter()
                .map(|(_, expected)| *expected)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn parser_infers_episode_and_skips_greyed_or_missing_video_rows() {
        let mut greyed = renderer(None, "Video", "greyed");
        greyed["musicResponsiveListItemRenderer"]["displayPolicy"] =
            Value::String("MUSIC_ITEM_RENDERER_DISPLAY_POLICY_GREY_OUT".to_owned());
        let missing = serde_json::json!({
            "musicResponsiveListItemRenderer": {"flexColumns": []}
        });
        let response = Value::Array(vec![renderer(None, "Episode", "episode"), greyed, missing]);

        let parsed = parse_video_search_value(&response);
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].music_video_type, YtmMusicVideoType::Episode);
        assert_eq!(parsed[0].song.video_id, "episode");
    }

    #[test]
    fn parser_caps_the_transfer_candidate_pool() {
        let response = Value::Array(
            (0..(super::super::transfer_api::TRANSFER_YTM_RESULT_LIMIT + 5))
                .map(|index| {
                    renderer(
                        Some("MUSIC_VIDEO_TYPE_OMV"),
                        "Video",
                        &format!("video-{index}"),
                    )
                })
                .collect(),
        );

        let parsed = parse_video_search_value(&response);
        assert_eq!(
            parsed.len(),
            super::super::transfer_api::TRANSFER_YTM_RESULT_LIMIT
        );
    }
}
