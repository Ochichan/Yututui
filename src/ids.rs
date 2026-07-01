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
