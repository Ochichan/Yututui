use crate::config::{AudioBackend, Config};
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioBackendCaps {
    pub supports_gapless: bool,
    pub supports_eq: bool,
    pub supports_device_selection: bool,
    pub supports_visualization_tap: bool,
    pub supports_stream_record: bool,
    pub owns_media_keys: bool,
}

impl AudioBackendCaps {
    pub fn mpv() -> Self {
        Self {
            supports_gapless: true,
            supports_eq: true,
            supports_device_selection: true,
            supports_visualization_tap: false,
            supports_stream_record: super::mpv::stream_record_supported(),
            owns_media_keys: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioRuntimeStatus {
    pub backend: AudioBackend,
    pub caps: AudioBackendCaps,
    pub mpv_program: String,
    pub mpv_version: Option<String>,
    pub mpv_available: bool,
    pub mpv_lifetime_supported: bool,
    pub mpv_lifetime_error: Option<String>,
    pub ytdlp_path: Option<PathBuf>,
    pub ytdlp_source: Option<&'static str>,
    pub ytdlp_version: Option<String>,
    pub output: Option<String>,
    pub device: Option<String>,
    pub cache_forward: String,
    pub cache_back: String,
    pub extra_args_count: usize,
    pub gapless: bool,
    pub media_controls_disabled_by_yututui: bool,
    pub local_crossfade: crate::crossfade::LocalCrossfade,
    /// Whether this build can overlap two local files, and why not when it cannot. Carried as
    /// the probe's own answer rather than a `supports_` bool beside `supports_gapless`, so the
    /// capability line and the reason line cannot disagree about it.
    pub overlap: crate::crossfade::OverlapSupport,
}

pub fn runtime_status(cfg: &Config) -> AudioRuntimeStatus {
    let audio = cfg.audio.runtime();
    let ytdlp = crate::tools::ytdlp_selection();
    let mpv_program = crate::tools::mpv_program();
    let mpv_available = crate::deps::on_path(&mpv_program);
    let mpv_lifetime_error = super::mpv::ensure_lifeline_supported()
        .err()
        .map(|error| format!("{error:#}"));
    AudioRuntimeStatus {
        backend: audio.backend,
        caps: AudioBackendCaps::mpv(),
        mpv_version: super::mpv::version_line(&mpv_program),
        mpv_available,
        mpv_lifetime_supported: mpv_available && mpv_lifetime_error.is_none(),
        mpv_lifetime_error,
        mpv_program,
        ytdlp_path: ytdlp.as_ref().map(|selection| selection.path.clone()),
        ytdlp_source: ytdlp.as_ref().map(|selection| selection.source.label()),
        ytdlp_version: ytdlp.and_then(|selection| selection.version),
        output: audio.mpv.output,
        device: audio.mpv.device,
        cache_forward: audio.mpv.cache_forward,
        cache_back: audio.mpv.cache_back,
        extra_args_count: audio.mpv.extra_args.len(),
        gapless: cfg.effective_gapless(),
        media_controls_disabled_by_yututui: super::mpv::media_controls_flag_supported(),
        local_crossfade: cfg.effective_local_crossfade(),
        overlap: crate::crossfade::overlap_support(),
    }
}
