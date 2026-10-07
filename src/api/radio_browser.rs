//! Radio Browser station parsing and query descriptors.

use serde_json::Value;

use super::{PlayableRef, Song};
use crate::search_source::SearchSource;
use crate::util::http;

pub const MIRRORS: [&str; 6] = [
    "all.api.radio-browser.info",
    "de1.api.radio-browser.info",
    "de2.api.radio-browser.info",
    "at1.api.radio-browser.info",
    "nl1.api.radio-browser.info",
    "fi1.api.radio-browser.info",
];

pub const USER_AGENT: &str = concat!(
    "yututui/",
    env!("CARGO_PKG_VERSION"),
    " (https://github.com/Ochichan/Yututui)"
);

pub const RESPONSE_MAX_BYTES: usize = 4 * 1024 * 1024;

fn collapse_whitespace(raw: &str) -> String {
    raw.split(|c: char| c.is_whitespace())
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// A station uuid that is safe to embed in a URL path and to key the catalog by.
pub fn is_valid_uuid(uuid: &str) -> bool {
    !uuid.is_empty()
        && uuid.len() <= 64
        && uuid
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// A uuid worth asking the directory about: real station ids are 36-char UUIDs, so a short
/// token is never resolvable and would only waste a request.
pub fn is_directory_uuid(uuid: &str) -> bool {
    is_valid_uuid(uuid) && uuid.len() >= 20
}

/// Exactly two ASCII letters, upper-cased; anything else (three-letter codes, blanks,
/// digits) is "unknown" — guessing a two-letter prefix would place stations on the wrong
/// continent.
pub fn normalize_country_code(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.len() == 2 && trimmed.chars().all(|c| c.is_ascii_alphabetic()) {
        trimmed.to_ascii_uppercase()
    } else {
        String::new()
    }
}

/// Community-supplied text reaches the panel and toasts directly, so it goes through the
/// same forbidden-character scrub as every other provider's metadata before whitespace is
/// collapsed and the length capped.
fn text_value(e: &Value, key: &str, max_chars: usize, fallback: Option<&str>) -> Option<String> {
    match e.get(key).and_then(Value::as_str) {
        Some(raw) => {
            let spaced = raw.replace(['\t', '\n', '\r'], " ");
            let scrubbed = super::sanitize_metadata_text(&spaced, max_chars);
            let collapsed = collapse_whitespace(&scrubbed);
            let text = collapsed.chars().take(max_chars).collect::<String>();
            if text.is_empty() {
                fallback.map(str::to_owned)
            } else {
                Some(text)
            }
        }
        None => fallback.map(str::to_owned),
    }
}

fn u64_value(e: &Value, key: &str) -> Option<u64> {
    match e.get(key) {
        Some(Value::Number(n)) => n.as_u64(),
        Some(Value::String(s)) => s.trim().parse::<u64>().ok(),
        _ => None,
    }
}

fn f64_value(e: &Value, key: &str) -> Option<f64> {
    match e.get(key) {
        Some(Value::Number(n)) => n.as_f64(),
        Some(Value::String(s)) => s.trim().parse::<f64>().ok(),
        _ => None,
    }
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct RadioStation {
    pub uuid: String,
    pub name: String,
    pub url: String,
    pub country: String,
    pub country_code: String,
    pub state: String,
    pub language: String,
    pub tags: String,
    pub codec: String,
    pub bitrate: u32,
    pub votes: u32,
    pub clicks: u32,
    pub lat: Option<f32>,
    pub lon: Option<f32>,
}

impl RadioStation {
    pub fn parse(e: &Value) -> Option<Self> {
        let uuid = e
            .get("stationuuid")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|id| is_valid_uuid(id))?
            .to_owned();
        let raw_url = e
            .get("url_resolved")
            .and_then(Value::as_str)
            .or_else(|| e.get("url").and_then(Value::as_str))?;
        if raw_url.chars().count() > 2048 {
            tracing::debug!(%uuid, "skipping radio station with oversized stream URL");
            return None;
        }
        let url = match super::validate_playable_url(SearchSource::RadioBrowser, raw_url) {
            Ok(url) => url,
            Err(error) => {
                tracing::debug!(%uuid, %error, "skipping radio station with invalid stream URL");
                return None;
            }
        };
        let lat = f64_value(e, "geo_lat")
            .filter(|lat| lat.is_finite() && (-90.0..=90.0).contains(lat))
            .map(|lat| lat as f32);
        let lon = f64_value(e, "geo_long")
            .filter(|lon| lon.is_finite() && (-180.0..=180.0).contains(lon))
            .map(|lon| lon as f32);
        // Null Island is how the directory spells "unknown" for some rows; treat it as missing
        // so the station gets a country estimate instead of a marker in the Gulf of Guinea.
        let (lat, lon) = match (lat, lon) {
            (Some(lat), Some(lon)) if lat != 0.0 || lon != 0.0 => (Some(lat), Some(lon)),
            _ => (None, None),
        };
        let country_code = normalize_country_code(
            e.get("countrycode")
                .and_then(Value::as_str)
                .unwrap_or_default(),
        );
        Some(Self {
            uuid,
            name: text_value(e, "name", 160, Some("Unknown station"))?,
            url,
            country: text_value(e, "country", 100, None).unwrap_or_default(),
            country_code,
            state: text_value(e, "state", 100, None).unwrap_or_default(),
            language: text_value(e, "language", 120, None).unwrap_or_default(),
            tags: text_value(e, "tags", 500, None).unwrap_or_default(),
            codec: text_value(e, "codec", 32, None).unwrap_or_default(),
            bitrate: u64_value(e, "bitrate").unwrap_or(0).min(u32::MAX as u64) as u32,
            votes: u64_value(e, "votes").unwrap_or(0).min(u32::MAX as u64) as u32,
            clicks: u64_value(e, "clickcount")
                .or_else(|| u64_value(e, "clicks"))
                .unwrap_or(0)
                .min(u32::MAX as u64) as u32,
            lat,
            lon,
        })
    }

    /// Whether a stored row still satisfies every rule `parse` enforces on network rows.
    pub fn is_valid(&self) -> bool {
        let coords_ok = match (self.lat, self.lon) {
            (Some(lat), Some(lon)) => {
                lat.is_finite()
                    && lon.is_finite()
                    && (-90.0..=90.0).contains(&lat)
                    && (-180.0..=180.0).contains(&lon)
            }
            (None, None) => true,
            _ => false,
        };
        is_valid_uuid(&self.uuid)
            && coords_ok
            && self.name.chars().count() <= 160
            && self.country.chars().count() <= 100
            && self.state.chars().count() <= 100
            && self.language.chars().count() <= 120
            && self.tags.chars().count() <= 500
            && self.codec.chars().count() <= 32
            && (self.country_code.is_empty()
                || normalize_country_code(&self.country_code) == self.country_code)
            && self.url.chars().count() <= 2048
            && super::validate_playable_url(SearchSource::RadioBrowser, &self.url).is_ok()
    }

    pub fn into_song(self) -> Song {
        let bitrate = if self.bitrate > 0 {
            format!("{}k", self.bitrate)
        } else {
            String::new()
        };
        let artist = [self.country.as_str(), self.codec.as_str(), bitrate.as_str()]
            .into_iter()
            .filter(|s| !s.trim().is_empty())
            .collect::<Vec<_>>()
            .join(" / ");
        let country_code = (!self.country_code.is_empty()).then_some(self.country_code.clone());
        let mut song = Song::from_source(
            SearchSource::RadioBrowser,
            self.uuid,
            self.name,
            artist,
            String::new(),
            PlayableRef::RadioStream { url: self.url },
        );
        song.radio_country_code = country_code;
        song
    }
}

pub fn parse_radio_station(e: &Value) -> Option<Song> {
    RadioStation::parse(e).map(RadioStation::into_song)
}

pub fn parse_station_list(json: &Value, max: usize) -> Vec<RadioStation> {
    let Some(entries) = json.as_array() else {
        return Vec::new();
    };
    let mut seen = std::collections::HashSet::new();
    let mut rows = Vec::new();
    for entry in entries {
        if rows.len() >= max {
            break;
        }
        if let Some(station) = RadioStation::parse(entry)
            && seen.insert(station.uuid.clone())
        {
            rows.push(station);
        }
    }
    rows
}

pub enum RadioQuery {
    World { limit: u32 },
    WorldMore { limit: u32 },
    Country { code: String, limit: u32 },
    Search { name: String, limit: u32 },
    ByUuid { uuids: Vec<String> },
    Click { uuid: String },
}

impl RadioQuery {
    pub fn by_uuid(uuids: Vec<String>) -> Option<Self> {
        if uuids.is_empty() || uuids.len() > 100 {
            return None;
        }
        let valid = uuids.iter().all(|uuid| is_directory_uuid(uuid));
        valid.then_some(Self::ByUuid { uuids })
    }

    pub fn country(code: &str, limit: u32) -> Option<Self> {
        let code = normalize_country_code(code);
        (!code.is_empty()).then_some(Self::Country { code, limit })
    }

    pub fn click(uuid: &str) -> Option<Self> {
        is_valid_uuid(uuid).then(|| Self::Click {
            uuid: uuid.to_owned(),
        })
    }

    pub fn path(&self) -> String {
        match self {
            Self::World { .. } | Self::WorldMore { .. } | Self::Search { .. } => {
                "/json/stations/search".to_owned()
            }
            Self::Country { code, .. } => {
                format!(
                    "/json/stations/bycountrycodeexact/{}",
                    normalize_country_code(code)
                )
            }
            Self::ByUuid { .. } => "/json/stations/byuuid".to_owned(),
            Self::Click { uuid } => format!("/json/url/{uuid}"),
        }
    }

    pub fn params(&self) -> Vec<(&'static str, String)> {
        match self {
            Self::World { limit } => vec![
                ("has_geo_info", "true".to_owned()),
                ("hidebroken", "true".to_owned()),
                ("order", "clickcount".to_owned()),
                ("reverse", "true".to_owned()),
                ("limit", limit.to_string()),
            ],
            Self::WorldMore { limit } => vec![
                ("hidebroken", "true".to_owned()),
                ("order", "random".to_owned()),
                ("limit", limit.to_string()),
            ],
            Self::Country { limit, .. } => vec![
                ("hidebroken", "true".to_owned()),
                ("order", "clickcount".to_owned()),
                ("reverse", "true".to_owned()),
                ("limit", limit.to_string()),
            ],
            Self::Search { name, limit } => vec![
                ("name", name.clone()),
                ("hidebroken", "true".to_owned()),
                ("order", "clickcount".to_owned()),
                ("reverse", "true".to_owned()),
                ("limit", limit.to_string()),
            ],
            Self::ByUuid { uuids } => vec![("uuids", uuids.join(","))],
            Self::Click { .. } => Vec::new(),
        }
    }

    pub fn max_records(&self) -> usize {
        match self {
            Self::World { limit }
            | Self::WorldMore { limit }
            | Self::Country { limit, .. }
            | Self::Search { limit, .. } => *limit as usize,
            Self::ByUuid { uuids } => uuids.len(),
            Self::Click { .. } => 1,
        }
    }
}

pub async fn request(client: &reqwest::Client, query: &RadioQuery) -> anyhow::Result<Value> {
    let path = query.path();
    let params = query.params();
    let max_records = query.max_records();
    let mut last_error = None;
    for mirror in MIRRORS {
        let url = format!("https://{mirror}{path}");
        let result = async {
            let resp = client.get(&url).query(&params).send().await?;
            let resp = resp.error_for_status()?;
            http::json_limited(resp, RESPONSE_MAX_BYTES).await as anyhow::Result<Value>
        }
        .await;
        let click = matches!(query, RadioQuery::Click { .. });
        match result {
            Ok(json) if !click && json.as_array().is_some_and(|a| a.len() <= max_records) => {
                return Ok(json);
            }
            Ok(json) if click && json.is_object() => return Ok(json),
            Ok(_) => {
                let error = anyhow::anyhow!("unexpected response shape from {mirror}");
                tracing::debug!(%mirror, error = %error, "radio browser mirror rejected");
                last_error = Some(error);
            }
            Err(error) => {
                tracing::debug!(%mirror, error = %format!("{error:#}"), "radio browser mirror failed");
                last_error = Some(error);
            }
        }
    }
    let last = last_error.unwrap_or_else(|| anyhow::anyhow!("no radio browser mirrors"));
    anyhow::bail!("all Radio Browser mirrors failed: {last:#}")
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = r#"{
        "stationuuid": "11111111-2222-3333-4444-555555555555",
        "name": "  Night\tRadio  Live ",
        "url_resolved": "https://stream.example.org/live",
        "country": "South Korea",
        "countrycode": "kr",
        "state": "Seoul",
        "language": "korean",
        "tags": "kpop news",
        "codec": "MP3",
        "bitrate": "128",
        "votes": 12,
        "clickcount": 34,
        "geo_lat": 37.5,
        "geo_long": 127.0
    }"#;

    #[test]
    fn parse_happy_path_with_geo() {
        let station = RadioStation::parse(&serde_json::from_str(FIXTURE).unwrap()).unwrap();
        assert_eq!(station.uuid, "11111111-2222-3333-4444-555555555555");
        assert_eq!(station.name, "Night Radio Live");
        assert_eq!(station.country_code, "KR");
        assert_eq!(station.bitrate, 128);
        assert_eq!(station.lat, Some(37.5));
        assert_eq!(station.lon, Some(127.0));
    }

    #[test]
    fn parse_requires_uuid_and_safe_url() {
        let mut v: Value = serde_json::from_str(FIXTURE).unwrap();
        v.as_object_mut().unwrap().remove("stationuuid");
        assert!(RadioStation::parse(&v).is_none());
        v.as_object_mut()
            .unwrap()
            .insert("stationuuid".into(), "u".into());
        v.as_object_mut()
            .unwrap()
            .insert("url_resolved".into(), "ftp://x".into());
        assert!(RadioStation::parse(&v).is_none());
    }

    #[test]
    fn parse_rejects_both_coords_when_one_out_of_range() {
        let mut v: Value = serde_json::from_str(FIXTURE).unwrap();
        v.as_object_mut()
            .unwrap()
            .insert("geo_lat".into(), 95.into());
        let station = RadioStation::parse(&v).unwrap();
        assert_eq!(station.lat, None);
        assert_eq!(station.lon, None);
    }

    #[test]
    fn parse_string_numbers_and_long_country_code() {
        let mut v: Value = serde_json::from_str(FIXTURE).unwrap();
        v.as_object_mut()
            .unwrap()
            .insert("countrycode".into(), "usa".into());
        let station = RadioStation::parse(&v).unwrap();
        assert_eq!(station.country_code, "");
        assert_eq!(station.bitrate, 128);
    }

    #[test]
    fn parse_rejects_oversized_url() {
        let long_url = format!("https://s.example.org/{}", "a".repeat(2100));
        let station = RadioStation::parse(&serde_json::json!({
            "stationuuid": "abc",
            "url": long_url,
        }));
        assert!(station.is_none());
    }

    #[test]
    fn into_song_matches_old_parser_shape() {
        let json: Value = serde_json::from_str(FIXTURE).unwrap();
        let song = RadioStation::parse(&json).unwrap().into_song();
        assert_eq!(song.video_id, "rad:11111111-2222-3333-4444-555555555555");
        assert_eq!(song.title, "Night Radio Live");
        assert_eq!(song.artist, "South Korea / MP3 / 128k");
        match song.playable {
            Some(PlayableRef::RadioStream { url }) => {
                assert_eq!(url, "https://stream.example.org/live")
            }
            other => panic!("unexpected playable: {other:?}"),
        }
    }

    #[test]
    fn parse_radio_station_equals_into_song() {
        let json: Value = serde_json::from_str(FIXTURE).unwrap();
        assert_eq!(
            parse_radio_station(&json),
            RadioStation::parse(&json).map(RadioStation::into_song)
        );
    }

    #[test]
    fn parse_station_list_dedupes_and_caps() {
        let json = serde_json::json!([
            {"stationuuid": "a", "url": "https://a", "name": "one"},
            {"stationuuid": "a", "url": "https://a", "name": "dup"},
            {"stationuuid": "b", "url": "https://b", "name": "two"},
            {"stationuuid": "c", "url": "https://c", "name": "three"},
        ]);
        let rows = parse_station_list(&json, 2);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].name, "one");
        assert_eq!(rows[1].name, "two");
    }

    #[test]
    fn query_params_are_exact() {
        assert_eq!(
            RadioQuery::World { limit: 500 }.params(),
            vec![
                ("has_geo_info", "true".to_owned()),
                ("hidebroken", "true".to_owned()),
                ("order", "clickcount".to_owned()),
                ("reverse", "true".to_owned()),
                ("limit", "500".to_owned()),
            ]
        );
        assert_eq!(
            RadioQuery::WorldMore { limit: 50 }.params(),
            vec![
                ("hidebroken", "true".to_owned()),
                ("order", "random".to_owned()),
                ("limit", "50".to_owned()),
            ]
        );
        assert_eq!(
            RadioQuery::Country {
                code: "KR".into(),
                limit: 40
            }
            .params(),
            vec![
                ("hidebroken", "true".to_owned()),
                ("order", "clickcount".to_owned()),
                ("reverse", "true".to_owned()),
                ("limit", "40".to_owned()),
            ]
        );
        assert_eq!(
            RadioQuery::Search {
                name: "jazz".into(),
                limit: 80
            }
            .params(),
            vec![
                ("name", "jazz".to_owned()),
                ("hidebroken", "true".to_owned()),
                ("order", "clickcount".to_owned()),
                ("reverse", "true".to_owned()),
                ("limit", "80".to_owned()),
            ]
        );
        assert_eq!(
            RadioQuery::ByUuid {
                uuids: vec!["u1".into(), "u2".into()]
            }
            .params(),
            vec![("uuids", "u1,u2".to_owned())]
        );
    }

    #[test]
    fn query_paths_and_caps() {
        assert_eq!(
            RadioQuery::World { limit: 1 }.path(),
            "/json/stations/search"
        );
        assert_eq!(
            RadioQuery::Country {
                code: "KR".into(),
                limit: 1
            }
            .path(),
            "/json/stations/bycountrycodeexact/KR"
        );
        assert_eq!(
            RadioQuery::ByUuid {
                uuids: vec!["u".into()]
            }
            .path(),
            "/json/stations/byuuid"
        );
        assert_eq!(RadioQuery::Click { uuid: "u".into() }.path(), "/json/url/u");
        assert_eq!(RadioQuery::Click { uuid: "u".into() }.max_records(), 1);
    }

    #[test]
    fn by_uuid_rejects_bad_input() {
        assert!(RadioQuery::by_uuid(vec![]).is_none());
        assert!(RadioQuery::by_uuid(vec!["short".into()]).is_none());
        assert!(RadioQuery::by_uuid((0..101).map(|i| format!("uuid-{i:020}")).collect()).is_none());
        assert!(RadioQuery::by_uuid(vec!["11111111-2222-3333-4444-555555555555".into()]).is_some());
    }
}
