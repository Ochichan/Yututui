use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use crate::config::{AudioBackend, Config};
use crate::util::process;

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
}

pub fn runtime_status(cfg: &Config) -> AudioRuntimeStatus {
    let audio = cfg.audio.runtime();
    let ytdlp = crate::tools::ytdlp_selection();
    let mpv_program = crate::tools::mpv_program();
    let mpv_available = crate::deps::on_path(&mpv_program);
    AudioRuntimeStatus {
        backend: audio.backend,
        caps: AudioBackendCaps::mpv(),
        mpv_version: mpv_version_line(&mpv_program),
        mpv_available,
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
    }
}

fn mpv_version_line(program: &str) -> Option<String> {
    if !crate::deps::on_path(program) {
        return None;
    }
    let mut cmd = process::std_command(program, process::ProcessProfile::Media);
    cmd.arg("--version").stdin(Stdio::null());
    process::std_output_limited(
        cmd,
        process::ProcessProfile::Media,
        Duration::from_secs(5),
        64 * 1024,
    )
    .ok()
    .filter(|out| out.status.success())
    .and_then(|out| String::from_utf8(out.stdout).ok())
    .and_then(|stdout| stdout.lines().next().map(str::to_owned))
}
