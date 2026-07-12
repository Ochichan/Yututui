use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Defaults shipped before cache-default migrations were revisioned.
pub const MPV_CACHE_FORWARD_LEGACY_DEFAULT: &str = "32MiB";
pub const MPV_CACHE_BACK_LEGACY_DEFAULT: &str = "8MiB";
/// Keep the migration policy dormant until every native OS selects the same smaller cache pair.
/// Revision zero is never persisted, so today's 32/8 users remain eligible for that future move.
pub const MPV_CACHE_DEFAULTS_REVISION: u64 = 0;

// Keep the runtime defaults at the legacy pair until the cross-platform benchmark selects a
// smaller common winner. Migration code below deliberately treats these as independent constants
// so changing only this pair later activates the already-tested exact-pair migration.
pub const MPV_CACHE_FORWARD_DEFAULT: &str = "32MiB";
pub const MPV_CACHE_BACK_DEFAULT: &str = "8MiB";

const fn unrevisioned_cache_defaults() -> u64 {
    0
}

const fn is_zero(value: &u64) -> bool {
    *value == 0
}

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
    /// Private persisted migration marker. Missing in pre-revision configs means revision 0.
    #[serde(
        rename = "_cache_defaults_revision",
        default = "unrevisioned_cache_defaults",
        skip_serializing_if = "is_zero"
    )]
    pub(crate) cache_defaults_revision: u64,
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
            cache_defaults_revision: MPV_CACHE_DEFAULTS_REVISION,
            output: None,
            device: None,
            cache_forward: MPV_CACHE_FORWARD_DEFAULT.to_owned(),
            cache_back: MPV_CACHE_BACK_DEFAULT.to_owned(),
            extra_args: Vec::new(),
        }
    }
}

impl MpvAudioConfig {
    pub(crate) fn set_cache_forward(&mut self, value: Option<String>) {
        self.cache_forward = value.unwrap_or_else(|| MPV_CACHE_FORWARD_DEFAULT.to_owned());
        self.mark_cache_defaults_current();
    }

    pub(crate) fn set_cache_back(&mut self, value: Option<String>) {
        self.cache_back = value.unwrap_or_else(|| MPV_CACHE_BACK_DEFAULT.to_owned());
        self.mark_cache_defaults_current();
    }

    /// Mark an explicit user edit or reset as belonging to the current defaults policy. Never
    /// lower a marker written by a future version of the application.
    pub(crate) fn mark_cache_defaults_current(&mut self) {
        self.mark_cache_defaults_revision(MPV_CACHE_DEFAULTS_REVISION);
    }

    fn mark_cache_defaults_revision(&mut self, target_revision: u64) {
        if self.cache_defaults_revision < target_revision {
            self.cache_defaults_revision = target_revision;
        }
    }

    /// Defensive typed fallback after raw migration and lenient schema recovery. Raw migration is
    /// preferred because it preserves unknown JSON fields; keeping the exact-pair rule here makes
    /// the in-memory result safe even when an unfamiliar raw marker shape cannot be patched.
    pub(super) fn migrate_cache_defaults(&mut self) -> bool {
        self.migrate_cache_defaults_to(
            MPV_CACHE_FORWARD_DEFAULT,
            MPV_CACHE_BACK_DEFAULT,
            MPV_CACHE_DEFAULTS_REVISION,
        )
    }

    fn migrate_cache_defaults_to(
        &mut self,
        target_forward: &str,
        target_back: &str,
        target_revision: u64,
    ) -> bool {
        if target_revision == 0 || self.cache_defaults_revision >= target_revision {
            return false;
        }
        if self.cache_defaults_revision == 0
            && self.cache_forward == MPV_CACHE_FORWARD_LEGACY_DEFAULT
            && self.cache_back == MPV_CACHE_BACK_LEGACY_DEFAULT
        {
            self.cache_forward = target_forward.to_owned();
            self.cache_back = target_back.to_owned();
        }
        self.cache_defaults_revision = target_revision;
        true
    }

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

/// Patch an existing raw `Config` JSON value without round-tripping it through the typed schema.
/// This retains unknown keys and custom cache values while adding the private revision marker.
pub(super) fn migrate_cache_defaults_json(value: &mut serde_json::Value) -> bool {
    migrate_cache_defaults_json_to(
        value,
        MPV_CACHE_FORWARD_DEFAULT,
        MPV_CACHE_BACK_DEFAULT,
        MPV_CACHE_DEFAULTS_REVISION,
    )
}

fn migrate_cache_defaults_json_to(
    value: &mut serde_json::Value,
    target_forward: &str,
    target_back: &str,
    target_revision: u64,
) -> bool {
    const REVISION_KEY: &str = "_cache_defaults_revision";

    if target_revision == 0 {
        return false;
    }

    let Some(mpv) = value
        .get_mut("audio")
        .and_then(|audio| audio.get_mut("mpv"))
        .and_then(serde_json::Value::as_object_mut)
    else {
        return false;
    };

    let revision = match mpv.get(REVISION_KEY) {
        None => 0,
        Some(value) => match value.as_u64() {
            Some(revision) => revision,
            // An unknown marker shape may belong to a newer schema. Preserve it instead of
            // guessing and potentially rewriting intentional user values.
            None => return false,
        },
    };
    if revision >= target_revision {
        return false;
    }

    let exact_legacy_pair = mpv.get("cache_forward").and_then(serde_json::Value::as_str)
        == Some(MPV_CACHE_FORWARD_LEGACY_DEFAULT)
        && mpv.get("cache_back").and_then(serde_json::Value::as_str)
            == Some(MPV_CACHE_BACK_LEGACY_DEFAULT);
    if revision == 0 && exact_legacy_pair {
        mpv.insert(
            "cache_forward".to_owned(),
            serde_json::Value::String(target_forward.to_owned()),
        );
        mpv.insert(
            "cache_back".to_owned(),
            serde_json::Value::String(target_back.to_owned()),
        );
    }
    mpv.insert(
        REVISION_KEY.to_owned(),
        serde_json::Value::from(target_revision),
    );
    true
}

#[cfg(test)]
pub(super) fn migrate_cache_defaults_json_to_for_test(
    value: &mut serde_json::Value,
    target_forward: &str,
    target_back: &str,
    target_revision: u64,
) -> bool {
    migrate_cache_defaults_json_to(value, target_forward, target_back, target_revision)
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

        assert_eq!(cfg.mpv.cache_defaults_revision, MPV_CACHE_DEFAULTS_REVISION);
        assert_eq!(runtime.backend, AudioBackend::Mpv);
        assert_eq!(runtime.mpv.output, None);
        assert_eq!(runtime.mpv.device, None);
        assert_eq!(runtime.mpv.cache_forward, MPV_CACHE_FORWARD_DEFAULT);
        assert_eq!(runtime.mpv.cache_back, MPV_CACHE_BACK_DEFAULT);
    }

    #[test]
    fn mpv_audio_runtime_normalizes_auto_blank_cache_and_args() {
        let cfg = MpvAudioConfig {
            cache_defaults_revision: MPV_CACHE_DEFAULTS_REVISION,
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

    #[test]
    fn revision_zero_is_absent_for_legacy_and_fresh_configs_until_selection() {
        let old: MpvAudioConfig =
            serde_json::from_str(r#"{"cache_forward":"32MiB","cache_back":"8MiB"}"#).unwrap();
        assert_eq!(old.cache_defaults_revision, 0);

        let fresh = MpvAudioConfig::default();
        assert_eq!(fresh.cache_defaults_revision, MPV_CACHE_DEFAULTS_REVISION);
        let json = serde_json::to_value(fresh).unwrap();
        assert!(json.get("_cache_defaults_revision").is_none());
    }

    #[test]
    fn dormant_production_policy_does_not_mark_or_rewrite_unrevisioned_values() {
        for mpv in [
            serde_json::json!({"cache_forward": "32MiB", "cache_back": "8MiB"}),
            serde_json::json!({"cache_forward": "64MiB", "cache_back": "3MiB"}),
            serde_json::json!({"cache_forward": "32MiB"}),
        ] {
            let mut value = serde_json::json!({"audio": {"mpv": mpv}});
            let before = value.clone();
            assert!(!migrate_cache_defaults_json(&mut value));
            assert_eq!(value, before);
        }

        let mut typed: MpvAudioConfig =
            serde_json::from_str(r#"{"cache_forward":"32MiB","cache_back":"8MiB"}"#).unwrap();
        assert!(!typed.migrate_cache_defaults());
        assert_eq!(typed.cache_defaults_revision, 0);
    }

    #[test]
    fn raw_exact_legacy_pair_migrates_to_future_selected_pair_atomically() {
        let mut value = serde_json::json!({
            "unknown_top": {"keep": [1, 2, 3]},
            "audio": {
                "unknown_audio": true,
                "mpv": {
                    "cache_forward": "32MiB",
                    "cache_back": "8MiB",
                    "unknown_mpv": {"keep": "exactly"}
                }
            }
        });

        assert!(migrate_cache_defaults_json_to(
            &mut value, "16MiB", "4MiB", 1
        ));
        assert_eq!(value["audio"]["mpv"]["cache_forward"], "16MiB");
        assert_eq!(value["audio"]["mpv"]["cache_back"], "4MiB");
        assert_eq!(value["audio"]["mpv"]["_cache_defaults_revision"], 1);
        assert_eq!(value["unknown_top"], serde_json::json!({"keep": [1, 2, 3]}));
        assert_eq!(value["audio"]["unknown_audio"], true);
        assert_eq!(
            value["audio"]["mpv"]["unknown_mpv"],
            serde_json::json!({"keep": "exactly"})
        );
    }

    #[test]
    fn raw_custom_partial_and_non_exact_pairs_are_only_marked() {
        for (forward, back) in [
            (Some("64MiB"), Some("8MiB")),
            (Some("32MiB"), None),
            (None, Some("8MiB")),
            (Some(" 32MiB"), Some("8MiB")),
            (Some("32MiB"), Some("8MiB ")),
            (Some("32mib"), Some("8MiB")),
            (Some("32MiB"), Some("8mib")),
        ] {
            let mut mpv = serde_json::Map::new();
            if let Some(forward) = forward {
                mpv.insert(
                    "cache_forward".to_owned(),
                    serde_json::Value::String(forward.to_owned()),
                );
            }
            if let Some(back) = back {
                mpv.insert(
                    "cache_back".to_owned(),
                    serde_json::Value::String(back.to_owned()),
                );
            }
            mpv.insert("unknown".to_owned(), serde_json::json!(["still", "here"]));
            let before_forward = mpv.get("cache_forward").cloned();
            let before_back = mpv.get("cache_back").cloned();
            let mut value = serde_json::json!({"audio": {"mpv": mpv}});

            assert!(migrate_cache_defaults_json_to(
                &mut value, "16MiB", "4MiB", 1
            ));
            assert_eq!(
                value["audio"]["mpv"].get("cache_forward"),
                before_forward.as_ref()
            );
            assert_eq!(
                value["audio"]["mpv"].get("cache_back"),
                before_back.as_ref()
            );
            assert_eq!(value["audio"]["mpv"]["_cache_defaults_revision"], 1);
            assert_eq!(
                value["audio"]["mpv"]["unknown"],
                serde_json::json!(["still", "here"])
            );
        }
    }

    #[test]
    fn raw_current_future_and_unknown_revisions_are_never_lowered_or_rewritten() {
        for revision in [
            serde_json::json!(1),
            serde_json::json!(u64::MAX),
            serde_json::json!("future-schema"),
        ] {
            let mut value = serde_json::json!({
                "audio": {"mpv": {
                    "_cache_defaults_revision": revision,
                    "cache_forward": "32MiB",
                    "cache_back": "8MiB"
                }}
            });
            let before = value.clone();
            assert!(!migrate_cache_defaults_json_to(
                &mut value, "16MiB", "4MiB", 1
            ));
            assert_eq!(value, before);
        }
    }

    #[test]
    fn simulated_typed_policy_marks_custom_pair_without_replacing_it() {
        let mut cfg: MpvAudioConfig =
            serde_json::from_str(r#"{"cache_forward":"64MiB","cache_back":"3MiB"}"#).unwrap();
        assert!(cfg.migrate_cache_defaults_to("16MiB", "4MiB", 1));
        assert_eq!(cfg.cache_forward, "64MiB");
        assert_eq!(cfg.cache_back, "3MiB");
        assert_eq!(cfg.cache_defaults_revision, 1);
    }

    #[test]
    fn typed_config_preserves_a_numeric_future_revision_across_edit_and_round_trip() {
        let mut cfg: MpvAudioConfig = serde_json::from_value(serde_json::json!({
            "_cache_defaults_revision": u64::MAX,
            "cache_forward": "96MiB",
            "cache_back": "12MiB"
        }))
        .unwrap();

        cfg.mark_cache_defaults_current();

        assert_eq!(cfg.cache_defaults_revision, u64::MAX);
        assert_eq!(cfg.cache_forward, "96MiB");
        assert_eq!(cfg.cache_back, "12MiB");
        let encoded = serde_json::to_value(cfg).unwrap();
        assert_eq!(
            encoded["_cache_defaults_revision"],
            serde_json::json!(u64::MAX)
        );
    }
}
