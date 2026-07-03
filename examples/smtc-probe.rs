//! GSMTC consumer probe — the machine-checkable half of the Windows SMTC QA
//! (docs/windows-smtc-completion-plan.md §5). Reads back what the OS actually
//! sees of the media session `ytt` publishes (identity, metadata, status,
//! timeline, rate) and can drive it exactly the way Phone Link / Bluetooth
//! AVRCP do — so `scripts/windows-smtc-manual-qa.ps1` asserts on JSON instead
//! of eyeballs. Windows-only; requires an interactive desktop session (the
//! GSMTC API fails under services/CI, same constraint as SMTC itself).
//!
//! Usage:
//!   cargo run --example smtc-probe                    # JSON dump of all sessions
//!   cargo run --example smtc-probe -- pause|play|next|prev
//!   cargo run --example smtc-probe -- seek <seconds>
//!
//! Control commands target the ytm-tui session (matched by AppUserModelID,
//! falling back to any source id containing "ytt" for pre-identity builds) and
//! print `{"accepted": …}` — exit 0 only when the session accepted the call.

#[cfg(not(windows))]
fn main() {
    eprintln!("smtc-probe only runs on Windows (it reads the GSMTC session list).");
    std::process::exit(2);
}

#[cfg(windows)]
fn main() {
    std::process::exit(win::run());
}

#[cfg(windows)]
mod win {
    use serde_json::json;
    use windows::Media::Control::{
        GlobalSystemMediaTransportControlsSession as Session,
        GlobalSystemMediaTransportControlsSessionManager as Manager,
        GlobalSystemMediaTransportControlsSessionPlaybackStatus as Status,
    };
    use windows::Media::MediaPlaybackAutoRepeatMode as RepeatMode;
    use ytm_tui::media::identity::APP_USER_MODEL_ID;

    pub fn run() -> i32 {
        let args: Vec<String> = std::env::args().skip(1).collect();
        match dispatch(&args) {
            Ok(code) => code,
            Err(e) => {
                eprintln!("smtc-probe failed: {e}");
                1
            }
        }
    }

    fn dispatch(args: &[String]) -> windows::core::Result<i32> {
        let manager = Manager::RequestAsync()?.join()?;
        match args.first().map(String::as_str) {
            None | Some("list") => list(&manager),
            Some("pause") => control(&manager, "pause", |s| s.TryPauseAsync()?.join()),
            Some("play") => control(&manager, "play", |s| s.TryPlayAsync()?.join()),
            Some("next") => control(&manager, "next", |s| s.TrySkipNextAsync()?.join()),
            Some("prev") => control(&manager, "prev", |s| s.TrySkipPreviousAsync()?.join()),
            Some("seek") => {
                let Some(seconds) = args.get(1).and_then(|raw| raw.parse::<f64>().ok()) else {
                    eprintln!("usage: smtc-probe seek <seconds>");
                    return Ok(2);
                };
                let hns = (seconds * 1e7) as i64;
                control(&manager, "seek", move |s| {
                    s.TryChangePlaybackPositionAsync(hns)?.join()
                })
            }
            Some(other) => {
                eprintln!("unknown command: {other} (expected list|pause|play|next|prev|seek)");
                Ok(2)
            }
        }
    }

    /// Dump every session as JSON. Per-session read failures become nulls, not
    /// errors — a QA run must still show the other sessions when one is flaky.
    fn list(manager: &Manager) -> windows::core::Result<i32> {
        let sessions = manager.GetSessions()?;
        let mut out = Vec::new();
        for i in 0..sessions.Size()? {
            let session = sessions.GetAt(i)?;
            out.push(describe(&session));
        }
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({ "sessions": out })).unwrap()
        );
        Ok(0)
    }

    fn describe(session: &Session) -> serde_json::Value {
        let source = session
            .SourceAppUserModelId()
            .map(|s| s.to_string())
            .unwrap_or_default();

        let (title, artist, album, thumbnail) = match session
            .TryGetMediaPropertiesAsync()
            .and_then(|op| op.join())
        {
            Ok(props) => (
                props.Title().map(|s| s.to_string()).ok(),
                props.Artist().map(|s| s.to_string()).ok(),
                props
                    .AlbumTitle()
                    .map(|s| s.to_string())
                    .ok()
                    .filter(|s| !s.is_empty()),
                props.Thumbnail().is_ok(),
            ),
            Err(_) => (None, None, None, false),
        };

        let (status, rate, shuffle, repeat) = match session.GetPlaybackInfo() {
            Ok(info) => (
                info.PlaybackStatus().ok().map(status_str),
                info.PlaybackRate().and_then(|r| r.Value()).ok(),
                info.IsShuffleActive().and_then(|r| r.Value()).ok(),
                info.AutoRepeatMode()
                    .and_then(|r| r.Value())
                    .ok()
                    .map(repeat_str),
            ),
            Err(_) => (None, None, None, None),
        };

        let timeline = session.GetTimelineProperties().ok().map(|t| {
            let secs = |v: windows::core::Result<windows::Foundation::TimeSpan>| {
                v.map(|ts| ts.Duration as f64 / 1e7).unwrap_or(-1.0)
            };
            json!({
                "start_s": secs(t.StartTime()),
                "end_s": secs(t.EndTime()),
                "position_s": secs(t.Position()),
                "max_seek_s": secs(t.MaxSeekTime()),
            })
        });

        json!({
            "source_app_user_model_id": source,
            "is_ytm_tui": is_ours(&source),
            "title": title,
            "artist": artist,
            "album": album,
            "thumbnail": thumbnail,
            "status": status,
            "rate": rate,
            "shuffle": shuffle,
            "repeat": repeat,
            "timeline": timeline,
        })
    }

    /// Send one transport call at the ytm-tui session, like an OS surface would.
    fn control(
        manager: &Manager,
        name: &str,
        call: impl Fn(&Session) -> windows::core::Result<bool>,
    ) -> windows::core::Result<i32> {
        let sessions = manager.GetSessions()?;
        for i in 0..sessions.Size()? {
            let session = sessions.GetAt(i)?;
            let source = session
                .SourceAppUserModelId()
                .map(|s| s.to_string())
                .unwrap_or_default();
            if !is_ours(&source) {
                continue;
            }
            let accepted = call(&session)?;
            println!(
                "{}",
                json!({ "command": name, "target": source, "accepted": accepted })
            );
            return Ok(if accepted { 0 } else { 1 });
        }
        eprintln!("no ytm-tui media session found (is ytt playing?)");
        Ok(3)
    }

    fn is_ours(source: &str) -> bool {
        source == APP_USER_MODEL_ID || source.to_ascii_lowercase().contains("ytt")
    }

    fn status_str(status: Status) -> &'static str {
        match status {
            Status::Closed => "closed",
            Status::Opened => "opened",
            Status::Changing => "changing",
            Status::Stopped => "stopped",
            Status::Playing => "playing",
            Status::Paused => "paused",
            _ => "unknown",
        }
    }

    fn repeat_str(mode: RepeatMode) -> &'static str {
        match mode {
            RepeatMode::None => "off",
            RepeatMode::Track => "one",
            RepeatMode::List => "all",
            _ => "unknown",
        }
    }
}
