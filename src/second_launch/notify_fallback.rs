//! Desktop notification for launches with no terminal to print to.
//!
//! A Finder/`.app`/GUI second launch has no tty: the focus ladder runs first, and this
//! notification then explains why no new player window appeared — the same convention the
//! platforms themselves use when they deny a focus request (GNOME's "app is ready"
//! notification, Windows' taskbar flash).
//!
//! macOS release builds compile the notify-rust backend out (`panic = "abort"` no-op in
//! `crate::notify`), so this module shells out to `osascript` there instead.

use crate::remote::proto::InstanceFile;
use crate::t;

pub fn notify_already_running(instance: Option<&InstanceFile>, focused: bool) {
    let body = notification_body(instance, focused);
    #[cfg(target_os = "macos")]
    {
        if macos_osascript_notification(&body) {
            return;
        }
    }
    // Blocking on purpose: the process exits right after this, and a detached notify
    // thread would be killed before the toast reaches the OS.
    crate::notify::emit_native_notification_blocking("YuTuTui!", &body);
}

/// "already running in <where>" with a best-effort location. Pure and tested.
pub(crate) fn notification_body(instance: Option<&InstanceFile>, focused: bool) -> String {
    let hint = instance.and_then(|instance| instance.host_terminal.as_ref());
    let location_en;
    let location_ko;
    if hint.is_some_and(|h| h.tmux.is_some()) {
        location_en = " in tmux".to_string();
        location_ko = " tmux에서".to_string();
    } else if let Some(name) = hint.and_then(|h| pretty_terminal_name(h.term_program.as_deref())) {
        location_en = format!(" in {name}");
        location_ko = format!(" {name}에서");
    } else {
        location_en = " in another terminal".to_string();
        location_ko = " 다른 터미널에서".to_string();
    }
    let mut body = t!(
        format!("YuTuTui! is already running{location_en}."),
        format!("YuTuTui!가 이미{location_ko} 실행 중입니다.")
    );
    if focused {
        body.push(' ');
        body.push_str(t!("Switched to it.", "해당 창으로 전환했습니다."));
    }
    // Same hygiene as the recording notifications: never let captured env strings smuggle
    // control bytes into a notification payload.
    body.chars().filter(|c| !c.is_control()).collect()
}

fn pretty_terminal_name(term_program: Option<&str>) -> Option<&'static str> {
    match term_program? {
        "Apple_Terminal" => Some("Terminal"),
        "iTerm.app" => Some("iTerm2"),
        "ghostty" | "Ghostty" => Some("Ghostty"),
        "WezTerm" => Some("WezTerm"),
        "WarpTerminal" => Some("Warp"),
        "kitty" => Some("kitty"),
        "vscode" => Some("VS Code"),
        "tmux" => Some("tmux"),
        _ => None,
    }
}

#[cfg(target_os = "macos")]
fn macos_osascript_notification(body: &str) -> bool {
    use crate::util::process::{ProcessProfile, std_command};
    let script = format!(
        "display notification {} with title \"YuTuTui!\"",
        applescript_string(body)
    );
    std_command("osascript", ProcessProfile::DesktopOpen)
        .args(["-e", &script])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// AppleScript string literal escaping (mirror of the desktop shell's helper — that one
/// lives behind the `desktop` feature and is not compiled into `ytt`).
#[cfg(any(target_os = "macos", test))]
fn applescript_string(input: &str) -> String {
    let escaped = input.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{escaped}\"")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote::proto::HostTerminalHint;

    fn instance_with(hint: HostTerminalHint) -> InstanceFile {
        InstanceFile {
            app_pid: 7,
            endpoint: "sock".into(),
            token: "tok".into(),
            created_unix: 1,
            mode: Default::default(),
            protocol_version: crate::remote::proto::PROTOCOL_VERSION,
            capabilities: Vec::new(),
            host_terminal: Some(hint),
        }
    }

    #[test]
    fn body_names_the_terminal_in_both_languages() {
        let instance = instance_with(HostTerminalHint {
            term_program: Some("WezTerm".into()),
            ..Default::default()
        });
        let _guard = crate::i18n::lock_for_test();
        for (lang, expect) in [
            (
                crate::i18n::Language::English,
                "already running in WezTerm.",
            ),
            (crate::i18n::Language::Korean, "WezTerm에서 실행 중입니다."),
        ] {
            crate::i18n::set_language(lang);
            assert!(
                notification_body(Some(&instance), false).contains(expect),
                "missing {expect:?}"
            );
        }
    }

    #[test]
    fn tmux_beats_terminal_name_and_unknown_degrades_gracefully() {
        let _guard = crate::i18n::lock_for_test();
        let tmuxed = instance_with(HostTerminalHint {
            term_program: Some("WezTerm".into()),
            tmux: Some("/tmp/tmux-1000/default,42,0".into()),
            ..Default::default()
        });
        assert!(notification_body(Some(&tmuxed), false).contains("in tmux"));

        let unknown = instance_with(HostTerminalHint::default());
        assert!(notification_body(Some(&unknown), false).contains("in another terminal"));
        assert!(notification_body(None, false).contains("in another terminal"));
    }

    #[test]
    fn focused_outcome_is_mentioned_and_control_bytes_are_stripped() {
        let _guard = crate::i18n::lock_for_test();
        let sneaky = instance_with(HostTerminalHint {
            term_program: Some("WezTerm".into()),
            tmux: Some("\x1b]0;evil\x07".into()),
            ..Default::default()
        });
        let body = notification_body(Some(&sneaky), true);
        assert!(body.contains("Switched to it."));
        assert!(!body.chars().any(char::is_control));
    }

    #[test]
    fn applescript_string_escapes_quotes_and_backslashes() {
        assert_eq!(applescript_string(r#"a"b\c"#), r#""a\"b\\c""#);
    }
}
