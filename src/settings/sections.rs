//! Section headers for each settings tab, as `(title, field_count)` in field order. The counts
//! must partition [`super::SettingsTab::fields`]; `SettingsState::sections` adjusts them for
//! fields hidden by mode.

use super::AtlasField;
use crate::eq;
use crate::t;
use crate::theme::ThemeRole;

pub(super) fn general_sections() -> Vec<(&'static str, usize)> {
    // Boundaries follow the existing field order, so row indices (and every test or coach
    // step that looks a General field up by position) are unchanged.
    vec![
        (t!("Basics", "기본", "基本"), 2),
        (t!("Search sources", "검색 소스", "検索ソース"), 10),
        (
            t!(
                "Files & library",
                "파일 · 라이브러리",
                "ファイル · ライブラリ"
            ),
            5,
        ),
        (t!("Display & input", "화면 · 입력", "表示 · 入力"), 4),
        (t!("Behavior", "동작", "動作"), 3),
        (t!("Data & reset", "데이터 · 초기화", "データ · 初期化"), 3),
    ]
}

pub(super) fn playback_sections() -> Vec<(&'static str, usize)> {
    vec![
        (
            t!("Now Playing", "현재 재생", "再生中"),
            10 + AtlasField::ALL.len(),
        ),
        (
            t!("Audio backend", "오디오 백엔드", "オーディオバックエンド"),
            3,
        ),
        (t!("EQ", "EQ", "EQ"), eq::BANDS + 2),
    ]
}

pub(super) fn graphics_sections() -> Vec<(&'static str, usize)> {
    // Animation sections and the fields within each are ordered by average resource
    // usage under typical use, lightest first (the user-facing sorting contract).
    vec![
        (t!("Theme", "테마", "テーマ"), 3),
        (t!("Colors", "색상", "カラー"), ThemeRole::ALL.len()),
        (
            t!(
                "Animation controls",
                "애니메이션 제어",
                "アニメーション制御"
            ),
            3,
        ),
        (
            t!("Event feedback", "이벤트 피드백", "イベントフィードバック"),
            7,
        ),
        (
            t!(
                "Interface motion",
                "인터페이스 동작",
                "インターフェース動作"
            ),
            7,
        ),
        (t!("Now playing", "현재 재생", "再生中"), 11),
        (t!("Ambient canvas", "배경 캔버스", "背景キャンバス"), 9),
        (
            t!("Canvas showpieces", "캔버스 쇼피스", "キャンバス演出"),
            6,
        ),
    ]
}

pub(super) fn accounts_sections() -> Vec<(&'static str, usize)> {
    vec![
        ("Last.fm", 3),
        ("ListenBrainz", 2),
        ("Spotify", 5),
        (t!("Scrobbling", "스크로블링", "スクロブル"), 1),
    ]
}

pub(super) fn ai_sections() -> Vec<(&'static str, usize)> {
    // Separate the chat/assistant config from the autoplay + curation trio so the
    // "Autoplay / Curating mode / Curating style" group reads as one intuitive unit.
    vec![
        (t!("Assistant", "어시스턴트", "アシスタント"), 6),
        (
            t!(
                "Autoplay & curation",
                "자동재생 · 큐레이팅",
                "自動再生 · キュレーション"
            ),
            3,
        ),
    ]
}
