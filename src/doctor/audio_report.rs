//! Audio-backend and mpv-lifetime diagnostics for `ytt doctor audio`.

use crate::{config, i18n};

pub(super) fn run(verbose: bool) -> i32 {
    let cfg = config::Config::load();
    i18n::set_language(cfg.effective_language());
    let lang = i18n::current();
    super::init_tools_sync(&cfg);

    let status = crate::player::backend::runtime_status(&cfg);
    let backend = status.backend.id();
    let ok = status.mpv_lifetime_supported && crate::tools::ytdlp_selection().is_some();

    println!(
        "{}",
        match lang {
            i18n::Language::Korean => "오디오 백엔드",
            i18n::Language::Japanese => "オーディオバックエンド",
            _ => "Audio backend",
        }
    );
    println!("  backend: {backend}");
    println!(
        "  mpv: {}",
        if status.mpv_available {
            match status.mpv_version.as_deref() {
                Some(version) => format!("{version} · {}", status.mpv_program),
                None => status.mpv_program.clone(),
            }
        } else {
            format!("missing · {}", status.mpv_program)
        }
    );
    println!(
        "  {}: {}",
        match lang {
            i18n::Language::Korean => "mpv 수명 보호",
            i18n::Language::Japanese => "mpvライフタイム保護",
            _ => "mpv lifetime protection",
        },
        mpv_lifetime_report(
            status.mpv_lifetime_supported,
            status.mpv_lifetime_error.as_deref(),
            lang
        )
    );
    match (
        &status.ytdlp_source,
        &status.ytdlp_version,
        &status.ytdlp_path,
    ) {
        (Some(source), version, Some(path)) => println!(
            "  yt-dlp: {source} {} · {}",
            version.as_deref().unwrap_or("?"),
            path.display()
        ),
        _ => println!(
            "  yt-dlp: {}",
            crate::tools::ytdlp_selection_error().unwrap_or_else(|| "missing".to_owned())
        ),
    }
    println!("  output: {}", status.output.as_deref().unwrap_or("auto"));
    println!("  device: {}", status.device.as_deref().unwrap_or("auto"));
    println!(
        "  cache: {} forward, {} back",
        status.cache_forward, status.cache_back
    );
    super::long_form_seek::print(&cfg.audio.mpv, verbose);
    println!("  gapless: {}", enabled_label(status.gapless));
    println!(
        "  media controls: {}",
        if status.media_controls_disabled_by_yututui {
            "mpv disabled; yututui owns OS session"
        } else {
            "mpv flag unsupported; yututui OS session still configured separately"
        }
    );
    println!(
        "  extra mpv args: {}{}",
        if status.extra_args_count == 0 {
            "none".to_owned()
        } else {
            status.extra_args_count.to_string()
        },
        match lang {
            i18n::Language::Korean => " · config `audio.mpv.extra_args` (재시작 후 적용)",
            i18n::Language::Japanese => " · config `audio.mpv.extra_args` (再起動後に適用)",
            _ => " · config `audio.mpv.extra_args` (next launch)",
        }
    );

    if verbose {
        println!();
        println!(
            "{}",
            match lang {
                i18n::Language::Korean => "기능",
                i18n::Language::Japanese => "機能",
                _ => "Capabilities",
            }
        );
        println!("  gapless: {}", yes_no(status.caps.supports_gapless));
        println!("  eq: {}", yes_no(status.caps.supports_eq));
        println!(
            "  device selection: {}",
            yes_no(status.caps.supports_device_selection)
        );
        println!(
            "  stream record: {}",
            yes_no(status.caps.supports_stream_record)
        );
        println!(
            "  visualization tap: {}",
            yes_no(status.caps.supports_visualization_tap)
        );
        println!("  owns media keys: {}", yes_no(status.caps.owns_media_keys));
    }

    if ok { 0 } else { 1 }
}

fn yes_no(value: bool) -> &'static str {
    if value { "yes" } else { "no" }
}

pub(super) fn mpv_lifetime_report(
    supported: bool,
    error: Option<&str>,
    lang: i18n::Language,
) -> String {
    if supported {
        if cfg!(unix) {
            match lang {
                i18n::Language::Korean => {
                    "준비됨 · heartbeat guardian + 상속 IPC lease (mpv 0.33 이상)".to_owned()
                }
                i18n::Language::Japanese => {
                    "準備完了 · heartbeat guardian + 継承IPC lease (mpv 0.33以上)".to_owned()
                }
                _ => "ready · heartbeat guardian + inherited IPC lease (mpv 0.33+)".to_owned(),
            }
        } else {
            match lang {
                i18n::Language::Korean => {
                    "준비됨 · heartbeat guardian + Windows Job Object".to_owned()
                }
                i18n::Language::Japanese => {
                    "準備完了 · heartbeat guardian + Windows Job Object".to_owned()
                }
                _ => "ready · heartbeat guardian + Windows Job Object".to_owned(),
            }
        }
    } else {
        let reason = error.unwrap_or("mpv lifetime protection unavailable");
        match lang {
            i18n::Language::Korean => format!("사용 불가 · {reason}"),
            i18n::Language::Japanese => format!("利用不可 · {reason}"),
            _ => format!("unavailable · {reason}"),
        }
    }
}

fn enabled_label(value: bool) -> &'static str {
    if value { "enabled" } else { "disabled" }
}

#[cfg(test)]
mod tests {
    use super::mpv_lifetime_report;

    #[test]
    fn mpv_lifetime_report_distinguishes_ready_and_unusable() {
        let ready = mpv_lifetime_report(true, None, crate::i18n::Language::English);
        if cfg!(unix) {
            assert!(ready.contains("inherited IPC lease (mpv 0.33+)"));
        } else {
            assert!(ready.contains("Windows Job Object"));
        }
        assert_eq!(
            mpv_lifetime_report(
                false,
                Some("probe rejected"),
                crate::i18n::Language::English
            ),
            "unavailable · probe rejected"
        );
    }
}
