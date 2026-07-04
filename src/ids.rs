//! Small domain identifier wrappers used at async boundaries.

use std::fmt;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct VideoId(String);

impl VideoId {
    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_string(self) -> String {
        self.0
    }
}

impl From<String> for VideoId {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl From<&str> for VideoId {
    fn from(value: &str) -> Self {
        Self(value.to_owned())
    }
}

impl fmt::Display for VideoId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct WatchUrl(String);

impl WatchUrl {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<String> for WatchUrl {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl From<&str> for WatchUrl {
    fn from(value: &str) -> Self {
        Self(value.to_owned())
    }
}

impl fmt::Display for WatchUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct StreamUrl(String);

impl StreamUrl {
    pub fn into_string(self) -> String {
        self.0
    }
}

impl From<String> for StreamUrl {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl From<&str> for StreamUrl {
    fn from(value: &str) -> Self {
        Self(value.to_owned())
    }
}

impl fmt::Display for StreamUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn video_id_preserves_value_across_accessors_display_and_serde() {
        let id = VideoId::from("dQw4w9WgXcQ");

        assert_eq!(id.as_str(), "dQw4w9WgXcQ");
        assert_eq!(id.to_string(), "dQw4w9WgXcQ");
        assert_eq!(serde_json::to_string(&id).unwrap(), r#""dQw4w9WgXcQ""#);
        assert_eq!(
            serde_json::from_str::<VideoId>(r#""dQw4w9WgXcQ""#).unwrap(),
            id
        );
        assert_eq!(id.into_string(), "dQw4w9WgXcQ");
    }

    #[test]
    fn watch_url_preserves_value_across_accessors_display_and_serde() {
        let url = WatchUrl::from("https://music.youtube.com/watch?v=dQw4w9WgXcQ");

        assert_eq!(
            url.as_str(),
            "https://music.youtube.com/watch?v=dQw4w9WgXcQ"
        );
        assert_eq!(
            url.to_string(),
            "https://music.youtube.com/watch?v=dQw4w9WgXcQ"
        );
        assert_eq!(
            serde_json::from_str::<WatchUrl>(r#""https://music.youtube.com/watch?v=dQw4w9WgXcQ""#)
                .unwrap(),
            url
        );
    }

    #[test]
    fn stream_url_preserves_value_across_display_and_serde() {
        let url = StreamUrl::from("https://example.invalid/stream.m4a");

        assert_eq!(url.to_string(), "https://example.invalid/stream.m4a");
        assert_eq!(
            serde_json::from_str::<StreamUrl>(r#""https://example.invalid/stream.m4a""#).unwrap(),
            url.clone()
        );
        assert_eq!(url.into_string(), "https://example.invalid/stream.m4a");
    }
}
