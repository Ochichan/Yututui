use std::io::Write;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::t;
use crate::util::process;

/// Copy `text` to the system clipboard. Mirrors `open_in_browser`: spawns the platform
/// clipboard tool with stdio nulled and pipes `text` to its stdin, so no native clipboard crate
/// is needed. macOS uses `pbcopy`, Windows `clip`; Linux tries `wl-copy` (Wayland) then
/// `xclip`/`xsel` (X11). Returns whether a helper accepted the handoff; callers that only want a
/// best-effort nicety can ignore the result.
pub(in crate::app) fn copy_to_clipboard(text: &str) -> bool {
    fn pipe(cmd: &mut Command, text: &str) -> bool {
        let Ok(mut child) = cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        else {
            return false;
        };

        if let Some(mut stdin) = child.stdin.take()
            && stdin.write_all(text.as_bytes()).is_err()
        {
            let _ = child.kill();
            let _ = child.wait();
            return false;
        }

        let start = Instant::now();
        loop {
            match child.try_wait() {
                Ok(Some(status)) => return status.success(),
                Ok(None) if start.elapsed() < Duration::from_millis(500) => {
                    std::thread::sleep(Duration::from_millis(25));
                }
                Ok(None) => return true,
                Err(_) => return false,
            }
        }
    }

    if cfg!(target_os = "macos") {
        pipe(
            &mut process::std_command("pbcopy", process::ProcessProfile::Clipboard),
            text,
        )
    } else if cfg!(target_os = "windows") {
        pipe(
            &mut process::std_command("clip", process::ProcessProfile::Clipboard),
            text,
        )
    } else if pipe(
        &mut process::std_command("wl-copy", process::ProcessProfile::Clipboard),
        text,
    ) || pipe(
        process::std_command("xclip", process::ProcessProfile::Clipboard)
            .args(["-selection", "clipboard"]),
        text,
    ) {
        true
    } else {
        pipe(
            process::std_command("xsel", process::ProcessProfile::Clipboard).arg("-ib"),
            text,
        )
    }
}

pub(in crate::app) fn spotify_auth_url_status(
    opened: bool,
    copied: bool,
    saved_path: Option<&std::path::Path>,
) -> String {
    if opened && copied {
        t!(
            "Approve YuTuTui! in the browser (link copied as fallback)",
            "브라우저에서 YuTuTui!를 승인해 주세요 (링크는 예비용으로 복사했어요)"
        )
        .to_owned()
    } else if opened {
        t!(
            "Approve YuTuTui! in the browser",
            "브라우저에서 YuTuTui!를 승인해 주세요"
        )
        .to_owned()
    } else if copied {
        t!(
            "Could not open browser; link copied. Paste it manually or run `ytt doctor --verbose`.",
            "브라우저를 열 수 없어요. 링크를 복사했으니 직접 붙여넣거나 `ytt doctor --verbose`를 실행해 주세요."
        )
        .to_owned()
    } else if let Some(path) = saved_path {
        if crate::i18n::is_korean() {
            format!(
                "브라우저/클립보드 실패. 인증 URL 저장됨: {}",
                path.display()
            )
        } else {
            format!(
                "Browser/clipboard failed; auth URL saved to {}",
                path.display()
            )
        }
    } else {
        t!(
            "Could not open browser or clipboard; run `ytt auth spotify --client-id …`.",
            "브라우저와 클립보드를 사용할 수 없어요. `ytt auth spotify --client-id …`를 실행해 주세요."
        )
        .to_owned()
    }
}
