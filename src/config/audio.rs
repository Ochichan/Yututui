use serde::{Deserialize, Deserializer, Serialize, Serializer};

pub const MPV_CACHE_FORWARD_DEFAULT: &str = "32MiB";
pub const MPV_CACHE_BACK_DEFAULT: &str = "8MiB";

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum AudioBackend {
    #[default]
    Mpv,
}

impl AudioBackend {
    pub fn id(self) -> &'static str {
        match self {
            Self::Mpv => "mpv",
        }
    }
}

impl Serialize for AudioBackend {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.id())
    }
}

impl<'de> Deserialize<'de> for AudioBackend {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let _ = String::deserialize(deserializer)?;
        Ok(Self::Mpv)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct AudioConfig {
    pub backend: AudioBackend,
    pub mpv: MpvAudioConfig,
}

impl Default for AudioConfig {
    fn default() -> Self {
        Self {
            backend: AudioBackend::Mpv,
            mpv: MpvAudioConfig::default(),
        }
    }
}

impl AudioConfig {
    pub fn effective_backend(&self) -> AudioBackend {
        self.backend
    }

    pub fn runtime(&self) -> AudioRuntimeConfig {
        AudioRuntimeConfig {
            backend: self.effective_backend(),
            mpv: self.mpv.effective(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct MpvAudioConfig {
    /// mpv audio output driver (`--ao`). `None`, blank, and `auto` leave mpv on its default.
    pub output: Option<String>,
    /// mpv audio device (`--audio-device`). `None`, blank, and `auto` leave mpv on its default.
    pub device: Option<String>,
    /// Forward demuxer cache size (`--demuxer-max-bytes`). Takes effect on the next player launch.
    pub cache_forward: String,
    /// Backward demuxer cache size (`--demuxer-max-back-bytes`). Takes effect on the next player launch.
    pub cache_back: String,
    /// Config-file escape hatch appended after structured audio args.
    /// Edit `audio.mpv.extra_args` in the config file; there is no settings UI for this yet.
    /// Takes effect on the next player launch (same as output/device/cache).
    pub extra_args: Vec<String>,
}

impl Default for MpvAudioConfig {
    fn default() -> Self {
        Self {
            output: None,
            device: None,
            cache_forward: MPV_CACHE_FORWARD_DEFAULT.to_owned(),
            cache_back: MPV_CACHE_BACK_DEFAULT.to_owned(),
            extra_args: Vec::new(),
        }
    }
}

impl MpvAudioConfig {
    pub fn effective(&self) -> MpvAudioRuntimeConfig {
        MpvAudioRuntimeConfig {
            output: normalize_optional(&self.output),
            device: normalize_optional(&self.device),
            cache_forward: normalize_cache(&self.cache_forward, MPV_CACHE_FORWARD_DEFAULT),
            cache_back: normalize_cache(&self.cache_back, MPV_CACHE_BACK_DEFAULT),
            extra_args: self
                .extra_args
                .iter()
                .filter_map(|arg| {
                    let arg = arg.trim();
                    (!arg.is_empty()).then(|| arg.to_owned())
                })
                .collect(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioRuntimeConfig {
    pub backend: AudioBackend,
    pub mpv: MpvAudioRuntimeConfig,
}

impl Default for AudioRuntimeConfig {
    fn default() -> Self {
        AudioConfig::default().runtime()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MpvAudioRuntimeConfig {
    pub output: Option<String>,
    pub device: Option<String>,
    pub cache_forward: String,
    pub cache_back: String,
    pub extra_args: Vec<String>,
}

impl Default for MpvAudioRuntimeConfig {
    fn default() -> Self {
        MpvAudioConfig::default().effective()
    }
}

fn normalize_optional(value: &Option<String>) -> Option<String> {
    let value = value.as_deref()?.trim();
    if value.is_empty() || value.eq_ignore_ascii_case("auto") {
        None
    } else {
        Some(value.to_owned())
    }
}

fn normalize_cache(value: &str, fallback: &str) -> String {
    let value = value.trim();
    if valid_mpv_cache_size(value) {
        value.to_owned()
    } else {
        fallback.to_owned()
    }
}

fn valid_mpv_cache_size(value: &str) -> bool {
    if value.is_empty() || value.starts_with('-') {
        return false;
    }
    let split = value
        .find(|ch: char| !ch.is_ascii_digit())
        .unwrap_or(value.len());
    if split == 0 {
        return false;
    }
    let suffix = &value[split..];
    suffix.is_empty()
        || matches!(
            suffix,
            "K" | "M" | "G" | "KB" | "MB" | "GB" | "KiB" | "MiB" | "GiB"
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audio_config_defaults_to_mpv_with_bounded_cache() {
        let cfg = AudioConfig::default();
        let runtime = cfg.runtime();

        assert_eq!(runtime.backend, AudioBackend::Mpv);
        assert_eq!(runtime.mpv.output, None);
        assert_eq!(runtime.mpv.device, None);
        assert_eq!(runtime.mpv.cache_forward, MPV_CACHE_FORWARD_DEFAULT);
        assert_eq!(runtime.mpv.cache_back, MPV_CACHE_BACK_DEFAULT);
    }

    #[test]
    fn mpv_audio_runtime_normalizes_auto_blank_cache_and_args() {
        let cfg = MpvAudioConfig {
            output: Some(" auto ".to_owned()),
            device: Some(" pipewire/thing ".to_owned()),
            cache_forward: "bad".to_owned(),
            cache_back: "64MiB".to_owned(),
            extra_args: vec!["".to_owned(), " --volume=0 ".to_owned()],
        };

        let runtime = cfg.effective();

        assert_eq!(runtime.output, None);
        assert_eq!(runtime.device.as_deref(), Some("pipewire/thing"));
        assert_eq!(runtime.cache_forward, MPV_CACHE_FORWARD_DEFAULT);
        assert_eq!(runtime.cache_back, "64MiB");
        assert_eq!(runtime.extra_args, ["--volume=0"]);
    }

    #[test]
    fn unknown_backend_deserializes_as_mpv() {
        let backend: AudioBackend = serde_json::from_str(r#""native""#).unwrap();
        assert_eq!(backend, AudioBackend::Mpv);
    }
}
