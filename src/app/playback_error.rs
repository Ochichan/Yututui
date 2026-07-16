use crate::tools::PlaybackFailureClass;

use super::*;

pub(in crate::app) fn skipped_status_for_failure(class: PlaybackFailureClass) -> String {
    match class {
        PlaybackFailureClass::Extraction => t!(
            "⚠ Couldn't resolve the stream (yt-dlp may be outdated) — skipped",
            "⚠ 스트림 해석 실패 (yt-dlp가 오래됐을 수 있음) — 건너뜀",
            "⚠ ストリームを解決できません (yt-dlp が古い可能性) — スキップ"
        )
        .to_owned(),
        PlaybackFailureClass::Http403 | PlaybackFailureClass::RateLimited => t!(
            "⚠ YouTube rejected the stream — skipped; run `ytt doctor --verbose`",
            "⚠ YouTube가 스트림을 거부함 — 건너뜀; `ytt doctor --verbose` 확인",
            "⚠ YouTube がストリームを拒否 — スキップ; `ytt doctor --verbose` を確認"
        )
        .to_owned(),
        PlaybackFailureClass::Network => t!(
            "⚠ Network error while opening stream — skipped",
            "⚠ 스트림 연결 네트워크 오류 — 건너뜀",
            "⚠ ストリーム接続のネットワークエラー — スキップ"
        )
        .to_owned(),
        PlaybackFailureClass::Unknown => t!(
            "⚠ Track unavailable — skipped to next",
            "⚠ 재생할 수 없는 곡 — 다음 곡으로 건너뜀",
            "⚠ 再生できない曲 — 次の曲へスキップ"
        )
        .to_owned(),
    }
}

pub(in crate::app) fn breaker_status_for_failure(class: PlaybackFailureClass) -> String {
    match class {
        PlaybackFailureClass::Extraction => t!(
            "Several tracks failed — run `ytt tools reset --playback`, `ytt tools update`, then `ytt doctor --verbose` if it continues.",
            "여러 곡 재생 실패 — `ytt tools reset --playback`, `ytt tools update` 실행 후 계속되면 `ytt doctor --verbose`를 확인하세요.",
            "複数の曲の再生に失敗 — `ytt tools reset --playback`、`ytt tools update` を実行し、続く場合は `ytt doctor --verbose` を確認してください。"
        )
        .to_owned(),
        PlaybackFailureClass::Http403 | PlaybackFailureClass::RateLimited => t!(
            "Several tracks failed — YouTube is rejecting streams. Run `ytt doctor --verbose`; check cookies and JS runtime.",
            "여러 곡 재생 실패 — YouTube가 스트림을 거부합니다. `ytt doctor --verbose`를 실행하고 쿠키와 JS runtime을 확인하세요.",
            "複数の曲の再生に失敗 — YouTube がストリームを拒否しています。`ytt doctor --verbose` を実行し、Cookie と JS runtime を確認してください。"
        )
        .to_owned(),
        PlaybackFailureClass::Network => t!(
            "Several tracks failed — check your connection, then run `ytt doctor --verbose` if it continues.",
            "여러 곡 재생 실패 — 연결을 확인하고 계속되면 `ytt doctor --verbose`를 실행하세요.",
            "複数の曲の再生に失敗 — 接続を確認し、続く場合は `ytt doctor --verbose` を実行してください。"
        )
        .to_owned(),
        PlaybackFailureClass::Unknown => t!(
            "Several tracks failed to play — stopped. Check your connection, or sign in (cookies) for gated tracks.",
            "여러 곡 재생에 실패해서 중단했어요. 연결을 확인하거나, 제한된 곡은 로그인(쿠키)하세요.",
            "複数の曲の再生に失敗したため停止しました。接続を確認するか、制限付きの曲はログイン(Cookie)してください。"
        )
        .to_owned(),
    }
}

pub(in crate::app) fn playback_error_status_for_failure(
    class: PlaybackFailureClass,
    error: &str,
) -> String {
    match class {
        PlaybackFailureClass::Extraction => t!(
            "Playback error: stream resolution failed; run `ytt tools update`, then `ytt doctor --verbose`",
            "재생 오류: 스트림 해석 실패; `ytt tools update` 후 `ytt doctor --verbose`를 실행하세요",
            "再生エラー: ストリーム解決に失敗; `ytt tools update` の後 `ytt doctor --verbose` を実行してください"
        )
        .to_owned(),
        PlaybackFailureClass::Http403 | PlaybackFailureClass::RateLimited => t!(
            "Playback error: YouTube rejected the stream; run `ytt doctor --verbose`, check cookies and JS runtime",
            "재생 오류: YouTube가 스트림을 거부함; `ytt doctor --verbose`를 실행하고 쿠키와 JS runtime을 확인하세요",
            "再生エラー: YouTube がストリームを拒否; `ytt doctor --verbose` を実行し Cookie と JS runtime を確認してください"
        )
        .to_owned(),
        PlaybackFailureClass::Network => t!(
            "Playback error: network issue while opening stream",
            "재생 오류: 스트림 연결 네트워크 문제",
            "再生エラー: ストリーム接続のネットワーク問題"
        )
        .to_owned(),
        PlaybackFailureClass::Unknown => format!(
            "{}: {error}",
            t!("Playback error", "재생 오류", "再生エラー")
        ),
    }
}
