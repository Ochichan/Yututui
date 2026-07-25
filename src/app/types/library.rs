//! Local-library navigation types.

use crate::t;

/// The lists in the library view.
#[derive(Debug, Default, PartialEq, Eq, Clone, Copy)]
pub enum LibraryTab {
    #[default]
    All,
    Favorites,
    History,
    RadioFavorites,
    Radio,
    Downloads,
    Playlists,
}

impl LibraryTab {
    pub const NORMAL: [Self; 5] = [
        Self::All,
        Self::Favorites,
        Self::History,
        Self::Downloads,
        Self::Playlists,
    ];

    pub const RADIO_MODE: [Self; 2] = [Self::RadioFavorites, Self::Radio];

    pub fn label(self) -> &'static str {
        match self {
            Self::All => t!("All", "전체", "すべて"),
            Self::Favorites => t!("Favorites", "즐겨찾기", "お気に入り"),
            Self::History => t!("History", "기록", "履歴"),
            Self::RadioFavorites => t!("Radio Likes", "라디오 좋아요", "ラジオ高評価"),
            Self::Radio => t!("Radio History", "라디오 히스토리", "ラジオ履歴"),
            Self::Downloads => t!("Downloads", "다운로드", "ダウンロード"),
            Self::Playlists => t!("Playlists", "플레이리스트", "プレイリスト"),
        }
    }

    pub fn compact_label(self) -> &'static str {
        match self {
            Self::All => t!("All", "전체", "すべて"),
            Self::Favorites => t!("Fav", "즐겨찾기", "お気に入り"),
            Self::History => t!("Hist", "기록", "履歴"),
            Self::RadioFavorites => t!("R-Like", "라디오 좋아요", "ラジオ高評価"),
            Self::Radio => t!("R-Hist", "라디오 기록", "ラジオ履歴"),
            Self::Downloads => t!("Down", "다운", "DL"),
            Self::Playlists => t!("Lists", "플리", "リスト"),
        }
    }
}
