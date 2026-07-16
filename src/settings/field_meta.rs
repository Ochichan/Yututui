//! Per-[`Field`] metadata: widget kind, animation-flag mapping, display label, secrecy.
//! Split out of `settings.rs` so the field tables can keep growing (the animation pack
//! alone is 41 toggles) without blowing the parent file's size budget.

use crate::config::AnimationsConfig;
use crate::t;

use super::{Field, FieldKind, freq_label};

impl Field {
    pub fn kind(self) -> FieldKind {
        match self {
            Field::CookiesFile
            | Field::DownloadDir
            | Field::LocalMusicRoot
            | Field::AudioMpvOutput
            | Field::AudioMpvDevice
            | Field::AudioMpvCacheForward
            | Field::AudioMpvCacheBack
            | Field::AudiusAppName
            | Field::JamendoClientId
            | Field::ApiKey
            | Field::ListenBrainzToken
            | Field::SpotifyClientId
            | Field::SpotifyRedirectPort
            | Field::ThemeColor(_) => FieldKind::Text,
            Field::BeginnerMode
            | Field::Mouse
            | Field::AlbumArt
            | Field::LocalIncludeDownloadDir
            | Field::LocalMusicRootRecursive
            | Field::AutoplayOnStart
            | Field::EnqueueNext
            | Field::SearchYoutube
            | Field::SearchSoundCloud
            | Field::SearchAudius
            | Field::SearchJamendo
            | Field::SearchInternetArchive
            | Field::SearchRadioBrowser
            | Field::BigText
            | Field::MouseWheelVolume
            | Field::Gapless
            | Field::MediaControls
            | Field::AutoContinueVideos
            | Field::UpdateCheck
            | Field::AutoplayStreaming
            | Field::Normalize
            | Field::AiEnabled
            | Field::RomanizedTitles
            | Field::RetroMode
            | Field::BackgroundNone
            | Field::AnimPauseUnfocused
            | Field::AnimMaster
            | Field::AnimTitle
            | Field::AnimHeart
            | Field::AnimSeekbar
            | Field::AnimSpinner
            | Field::AnimEqBars
            | Field::AnimControls
            | Field::AnimBorder
            | Field::AnimTrackIntro
            | Field::AnimLyrics
            | Field::AnimToast
            | Field::AnimVolumeFlash
            | Field::AnimLikeBurst
            | Field::AnimSeekFlash
            | Field::AnimSelection
            | Field::AnimStagger
            | Field::AnimCaret
            | Field::AnimTabs
            | Field::AnimPopupFade
            | Field::AnimActivity
            | Field::AnimAboutFx
            | Field::AnimPauseFlash
            | Field::AnimErrorShake
            | Field::AnimTimeGlow
            | Field::AnimProgressSparkle
            | Field::AnimBorderChase
            | Field::AnimRain
            | Field::AnimDonut
            | Field::AnimVisualizer
            | Field::AnimStarfield
            | Field::AnimBounce
            | Field::AnimComets
            | Field::AnimSnow
            | Field::AnimFireflies
            | Field::AnimCube
            | Field::AnimAquarium
            | Field::AnimWaves
            | Field::AnimFireworks
            | Field::AnimLife
            | Field::AnimPipes
            | Field::AnimPlasma
            | Field::LastfmEnabled
            | Field::LastfmLoveSync
            | Field::ListenBrainzEnabled
            | Field::ScrobbleLocalFiles => FieldKind::Toggle,
            Field::Language
            | Field::SearchSource
            | Field::StreamingSource
            | Field::EqPreset
            | Field::GeminiModel
            | Field::ThemePreset
            | Field::CuratingMode
            | Field::DjGemLanguage
            | Field::AudioBackend
            | Field::LongFormSeekOptimization
            | Field::VideoLayout
            | Field::AlbumArtQuality
            | Field::PlayerBarPosition
            | Field::SpotifyImportMode
            | Field::StreamingMode => FieldKind::Select,
            Field::Speed | Field::SeekInterval | Field::Band(_) | Field::AnimFps => {
                FieldKind::Slider
            }
            Field::ExportPersonalData
            | Field::ResetKeybindings
            | Field::ResetAll
            | Field::ClearRomanizedTitleCache
            | Field::RadioRecording
            | Field::AudioOutput
            | Field::LastfmConnect
            | Field::SpotifyConnect
            | Field::SpotifyImport => FieldKind::Button,
        }
    }

    /// For an animation toggle field, a mutable handle to its flag inside an
    /// [`AnimationsConfig`]; `None` for any non-animation field. This single mapping is the
    /// source of truth used for both rendering the checkbox and flipping it on input — so the
    /// 41 toggles (master + 40 effects) never drift out of sync across the display / toggle /
    /// persist paths.
    pub(crate) fn anim_flag(self, a: &mut AnimationsConfig) -> Option<&mut bool> {
        Some(match self {
            Field::AnimMaster => &mut a.master,
            Field::AnimTitle => &mut a.title,
            Field::AnimHeart => &mut a.heart,
            Field::AnimSeekbar => &mut a.seekbar,
            Field::AnimSpinner => &mut a.spinner,
            Field::AnimEqBars => &mut a.eq_bars,
            Field::AnimControls => &mut a.controls,
            Field::AnimBorder => &mut a.border,
            Field::AnimTrackIntro => &mut a.track_intro,
            Field::AnimLyrics => &mut a.lyrics,
            Field::AnimToast => &mut a.toast,
            Field::AnimVolumeFlash => &mut a.volume_flash,
            Field::AnimLikeBurst => &mut a.like_burst,
            Field::AnimSeekFlash => &mut a.seek_flash,
            Field::AnimSelection => &mut a.selection,
            Field::AnimStagger => &mut a.stagger,
            Field::AnimCaret => &mut a.caret,
            Field::AnimTabs => &mut a.tabs,
            Field::AnimPopupFade => &mut a.popup_fade,
            Field::AnimActivity => &mut a.activity,
            Field::AnimAboutFx => &mut a.about_fx,
            Field::AnimPauseFlash => &mut a.pause_flash,
            Field::AnimErrorShake => &mut a.error_shake,
            Field::AnimTimeGlow => &mut a.time_glow,
            Field::AnimProgressSparkle => &mut a.progress_sparkle,
            Field::AnimBorderChase => &mut a.border_chase,
            Field::AnimRain => &mut a.rain,
            Field::AnimDonut => &mut a.donut,
            Field::AnimVisualizer => &mut a.visualizer,
            Field::AnimStarfield => &mut a.starfield,
            Field::AnimBounce => &mut a.bounce,
            Field::AnimComets => &mut a.comets,
            Field::AnimSnow => &mut a.snow,
            Field::AnimFireflies => &mut a.fireflies,
            Field::AnimCube => &mut a.cube,
            Field::AnimAquarium => &mut a.aquarium,
            Field::AnimWaves => &mut a.waves,
            Field::AnimFireworks => &mut a.fireworks,
            Field::AnimLife => &mut a.life,
            Field::AnimPipes => &mut a.pipes,
            Field::AnimPlasma => &mut a.plasma,
            _ => return None,
        })
    }

    pub fn label(self) -> String {
        match self {
            Field::BeginnerMode => t!("Beginner Mode", "비기너 모드", "ビギナーモード").to_owned(),
            Field::Language => t!("Language", "언어", "言語").to_owned(),
            Field::SearchSource => t!("Search source", "검색 소스", "検索ソース").to_owned(),
            Field::StreamingSource => {
                t!("Streaming source", "추천 소스", "おすすめソース").to_owned()
            }
            Field::SearchYoutube => {
                t!("Source: YouTube", "소스: YouTube", "ソース: YouTube").to_owned()
            }
            Field::SearchSoundCloud => t!(
                "Source: SoundCloud",
                "소스: SoundCloud",
                "ソース: SoundCloud"
            )
            .to_owned(),
            Field::SearchAudius => {
                t!("Source: Audius", "소스: Audius", "ソース: Audius").to_owned()
            }
            Field::AudiusAppName => {
                t!("Audius app name", "Audius 앱 이름", "Audiusアプリ名").to_owned()
            }
            Field::SearchJamendo => {
                t!("Source: Jamendo", "소스: Jamendo", "ソース: Jamendo").to_owned()
            }
            Field::JamendoClientId => t!(
                "Jamendo client_id",
                "Jamendo client_id",
                "Jamendo client_id"
            )
            .to_owned(),
            Field::SearchInternetArchive => t!(
                "Source: Internet Archive",
                "소스: Internet Archive",
                "ソース: Internet Archive"
            )
            .to_owned(),
            Field::SearchRadioBrowser => t!(
                "Source: Radio Browser",
                "소스: Radio Browser",
                "ソース: Radio Browser"
            )
            .to_owned(),
            Field::CookiesFile => t!("Cookies file", "쿠키 파일", "Cookieファイル").to_owned(),
            Field::DownloadDir => t!("Download dir", "다운로드 폴더", "ダウンロード先").to_owned(),
            Field::LocalIncludeDownloadDir => t!(
                "Local: include downloads",
                "로컬: 다운로드 포함",
                "ローカル: ダウンロード含む"
            )
            .to_owned(),
            Field::LocalMusicRoot => t!(
                "Local: music folder",
                "로컬: 음악 폴더",
                "ローカル: 音楽フォルダー"
            )
            .to_owned(),
            Field::LocalMusicRootRecursive => t!(
                "Local: scan subfolders",
                "로컬: 하위 폴더 스캔",
                "ローカル: サブフォルダー含む"
            )
            .to_owned(),
            Field::Mouse => t!(
                "Mouse (next launch)",
                "마우스 (재시작 후 적용)",
                "マウス (次回起動時)"
            )
            .to_owned(),
            Field::AlbumArt => t!("Album art", "앨범 아트", "アルバムアート").to_owned(),
            Field::AlbumArtQuality => {
                t!("Album art quality", "앨범 아트 화질", "アルバムアート画質").to_owned()
            }
            Field::PlayerBarPosition => t!(
                "Player bar position",
                "플레이어 바 위치",
                "プレイヤーバー位置"
            )
            .to_owned(),
            Field::AutoplayOnStart => t!(
                "Autoplay on launch",
                "앱 시작 시 자동재생",
                "起動時に自動再生"
            )
            .to_owned(),
            Field::EnqueueNext => {
                t!("Enqueue as next", "큐 추가: 다음 곡", "キュー追加: 次の曲").to_owned()
            }
            Field::UpdateCheck => {
                t!("Check for updates", "업데이트 확인", "アップデート確認").to_owned()
            }
            Field::ExportPersonalData => t!(
                "Export personal data",
                "개인 데이터 내보내기",
                "個人データのエクスポート"
            )
            .to_owned(),
            Field::ResetKeybindings => {
                t!("Reset keybindings", "단축키 초기화", "ショートカット初期化").to_owned()
            }
            Field::ResetAll => t!(
                "Reset all settings",
                "모든 설정 초기화",
                "すべての設定を初期化"
            )
            .to_owned(),
            Field::BigText => t!("Large text", "큰 글자 모드", "大きな文字モード").to_owned(),
            Field::Speed => t!("Playback speed", "재생 속도", "再生速度").to_owned(),
            Field::SeekInterval => t!("Seek interval", "탐색 간격", "シーク間隔").to_owned(),
            Field::MouseWheelVolume => {
                t!("Wheel volume", "휠 볼륨 조절", "ホイール音量調節").to_owned()
            }
            Field::Gapless => t!(
                "Gapless (next launch)",
                "갭리스 (재시작 후 적용)",
                "ギャップレス (次回起動時)"
            )
            .to_owned(),
            Field::MediaControls => t!(
                "OS media controls",
                "OS 미디어 컨트롤",
                "OSメディアコントロール"
            )
            .to_owned(),
            Field::AutoContinueVideos => t!(
                "Auto-continue videos",
                "영상 자동 이어재생",
                "動画の自動連続再生"
            )
            .to_owned(),
            Field::VideoLayout => t!("Video window", "영상 창", "動画ウィンドウ").to_owned(),
            Field::RadioRecording => t!("Radio recording", "라디오 녹음", "ラジオ録音").to_owned(),
            Field::AudioBackend => {
                t!("Backend (mpv)", "백엔드 (mpv)", "バックエンド (mpv)").to_owned()
            }
            Field::AudioOutput => t!("Audio output", "오디오 출력", "オーディオ出力").to_owned(),
            Field::AudioMpvOutput => t!(
                "mpv output (next launch)",
                "mpv 출력 (재시작 후 적용)",
                "mpv出力 (次回起動時)"
            )
            .to_owned(),
            Field::AudioMpvDevice => t!(
                "mpv device (next launch)",
                "mpv 장치 (재시작 후 적용)",
                "mpvデバイス (次回起動時)"
            )
            .to_owned(),
            Field::LongFormSeekOptimization => {
                t!("Long-form seek", "긴 미디어 탐색", "長尺メディアのシーク").to_owned()
            }
            Field::AudioMpvCacheForward => t!(
                "Cache forward (next launch)",
                "앞쪽 캐시 (재시작 후 적용)",
                "前方キャッシュ (次回起動時)"
            )
            .to_owned(),
            Field::AudioMpvCacheBack => t!(
                "Cache back (next launch)",
                "뒤쪽 캐시 (재시작 후 적용)",
                "後方キャッシュ (次回起動時)"
            )
            .to_owned(),
            Field::AutoplayStreaming => t!("Autoplay", "자동재생", "自動再生").to_owned(),
            Field::CuratingMode => {
                t!("Curating mode", "큐레이팅 방식", "キュレーション方式").to_owned()
            }
            Field::StreamingMode => t!(
                "Curating style",
                "큐레이팅 스타일",
                "キュレーションスタイル"
            )
            .to_owned(),
            Field::EqPreset => t!("Preset", "프리셋", "プリセット").to_owned(),
            Field::Band(i) => format!("{:>5}", freq_label(i)),
            Field::Normalize => {
                t!("Normalize (loudness)", "음량 평준화", "音量の正規化").to_owned()
            }
            Field::AiEnabled => t!("DJ Gem chat", "DJ Gem 채팅", "DJ Gemチャット").to_owned(),
            Field::GeminiModel => t!("Model", "모델", "モデル").to_owned(),
            Field::ApiKey => t!("API key", "API 키", "APIキー").to_owned(),
            Field::DjGemLanguage => t!("Reply language", "답변 언어", "応答言語").to_owned(),
            Field::RomanizedTitles => t!(
                "Romanized titles",
                "제목 로마자 표기",
                "タイトルのローマ字表記"
            )
            .to_owned(),
            Field::ClearRomanizedTitleCache => t!(
                "Clear romanized title cache",
                "로마자 제목 캐시 삭제",
                "ローマ字キャッシュ削除"
            )
            .to_owned(),
            Field::RetroMode => t!("Retro mode", "레트로 모드", "レトロモード").to_owned(),
            Field::ThemePreset => t!("Preset", "프리셋", "プリセット").to_owned(),
            Field::BackgroundNone => t!("Background: None", "배경 없음", "背景なし").to_owned(),
            Field::ThemeColor(role) => role.label().to_owned(),
            Field::AnimMaster => t!(
                "Enable animations",
                "애니메이션 켜기",
                "アニメーション有効化"
            )
            .to_owned(),
            Field::AnimFps => t!("Frame rate", "프레임 레이트", "フレームレート").to_owned(),
            Field::AnimPauseUnfocused => t!(
                "Pause when unfocused",
                "포커스 없을 때 정지",
                "非フォーカス時に停止"
            )
            .to_owned(),
            Field::AnimTitle => t!("Title shimmer", "제목 반짝임", "タイトルの輝き").to_owned(),
            Field::AnimHeart => t!("Beating heart", "하트 박동", "ハートの鼓動").to_owned(),
            Field::AnimSeekbar => {
                t!("Seekbar glow", "탐색바 반짝임", "シークバーの輝き").to_owned()
            }
            Field::AnimSpinner => {
                t!("Now-playing spinner", "재생 스피너", "再生スピナー").to_owned()
            }
            Field::AnimEqBars => t!("EQ bars", "EQ 막대", "EQバー").to_owned(),
            Field::AnimControls => {
                t!("Control pulse", "컨트롤 펄스", "コントロールパルス").to_owned()
            }
            Field::AnimBorder => t!("Breathing border", "테두리 호흡", "枠線の呼吸").to_owned(),
            Field::AnimTrackIntro => t!(
                "Track intro reveal",
                "곡 시작 타이틀 등장",
                "曲開始のタイトル登場"
            )
            .to_owned(),
            Field::AnimLyrics => t!("Lyrics glow", "가사 글로우", "歌詞グロー").to_owned(),
            Field::AnimToast => t!(
                "Status typewriter",
                "상태 메시지 타자기",
                "状態のタイプライター"
            )
            .to_owned(),
            Field::AnimVolumeFlash => {
                t!("Volume flash", "볼륨 플래시", "音量フラッシュ").to_owned()
            }
            Field::AnimLikeBurst => {
                t!("Like heart burst", "좋아요 하트 팡", "高評価ハートバースト").to_owned()
            }
            Field::AnimSeekFlash => t!("Seek ripple", "탐색 물결", "シークの波紋").to_owned(),
            Field::AnimSelection => {
                t!("Selection breathing", "선택 줄 숨쉬기", "選択行の呼吸").to_owned()
            }
            Field::AnimStagger => {
                t!("List cascade", "목록 캐스케이드", "リストカスケード").to_owned()
            }
            Field::AnimCaret => t!("Caret blink", "커서 깜빡임", "カーソル点滅").to_owned(),
            Field::AnimTabs => t!("Tab pop", "탭 강조 팝", "タブのポップ").to_owned(),
            Field::AnimPopupFade => {
                t!("Popup fade-in", "팝업 페이드인", "ポップアップフェード").to_owned()
            }
            Field::AnimActivity => t!("Activity dots", "진행 표시 점", "進行表示ドット").to_owned(),
            Field::AnimAboutFx => {
                t!("About sparkles", "정보 카드 반짝임", "情報カードの輝き").to_owned()
            }
            Field::AnimPauseFlash => {
                t!("Pause light wave", "일시정지 파장", "一時停止の波").to_owned()
            }
            Field::AnimErrorShake => t!("Error shake", "오류 흔들림", "エラーの揺れ").to_owned(),
            Field::AnimTimeGlow => t!("Second-tick glow", "초침 글로우", "秒針グロー").to_owned(),
            Field::AnimProgressSparkle => {
                t!("Playhead sparkles", "재생 지점 불꽃", "再生位置の火花").to_owned()
            }
            Field::AnimBorderChase => t!("Border comet", "테두리 혜성", "枠線の彗星").to_owned(),
            Field::AnimRain => t!("Matrix rain", "매트릭스 비", "マトリックスの雨").to_owned(),
            Field::AnimDonut => t!("Spinning donut", "회전 도넛", "回転ドーナツ").to_owned(),
            Field::AnimVisualizer => {
                t!("Visualizer", "비주얼라이저", "ビジュアライザー").to_owned()
            }
            Field::AnimStarfield => t!("Starfield / notes", "별·음표", "星·音符").to_owned(),
            Field::AnimBounce => t!("Bouncing logo", "튕기는 로고", "跳ねるロゴ").to_owned(),
            Field::AnimComets => t!("Shooting stars", "별똥별", "流れ星").to_owned(),
            Field::AnimSnow => t!("Snowfall", "눈 내림", "降雪").to_owned(),
            Field::AnimFireflies => t!("Fireflies", "반딧불", "ホタル").to_owned(),
            Field::AnimCube => t!(
                "Wireframe cube",
                "와이어프레임 큐브",
                "ワイヤーフレームキューブ"
            )
            .to_owned(),
            Field::AnimAquarium => t!("Aquarium", "수족관", "アクアリウム").to_owned(),
            Field::AnimWaves => t!("Ocean waves", "파도", "海の波").to_owned(),
            Field::AnimFireworks => t!("Fireworks", "불꽃놀이", "花火").to_owned(),
            Field::AnimLife => t!("Game of Life", "생명 게임", "ライフゲーム").to_owned(),
            Field::AnimPipes => t!("Pipes", "파이프", "パイプ").to_owned(),
            Field::AnimPlasma => t!("Plasma field", "플라즈마", "プラズマ").to_owned(),
            Field::LastfmEnabled => t!(
                "Scrobble to Last.fm",
                "Last.fm 스크로블",
                "Last.fmスクロブル"
            )
            .to_owned(),
            Field::LastfmConnect => {
                t!("Last.fm account", "Last.fm 계정", "Last.fmアカウント").to_owned()
            }
            Field::LastfmLoveSync => t!(
                "Sync likes as loves",
                "좋아요를 love로 동기화",
                "高評価をloveと同期"
            )
            .to_owned(),
            Field::ListenBrainzEnabled => t!(
                "Scrobble to ListenBrainz",
                "ListenBrainz 스크로블",
                "ListenBrainzスクロブル"
            )
            .to_owned(),
            Field::ListenBrainzToken => {
                t!("User token", "사용자 토큰", "ユーザートークン").to_owned()
            }
            Field::ScrobbleLocalFiles => t!(
                "Scrobble local files",
                "로컬 파일 스크로블",
                "ローカル曲のスクロブル"
            )
            .to_owned(),
            Field::SpotifyClientId => t!("Client ID", "클라이언트 ID", "クライアントID").to_owned(),
            Field::SpotifyRedirectPort => {
                t!("Redirect port", "리다이렉트 포트", "リダイレクトポート").to_owned()
            }
            Field::SpotifyConnect => {
                t!("Spotify account", "Spotify 계정", "Spotifyアカウント").to_owned()
            }
            Field::SpotifyImportMode => t!(
                "Spotify import mode",
                "Spotify 가져오기 모드",
                "Spotifyインポートモード"
            )
            .to_owned(),
            Field::SpotifyImport => t!(
                "Import from Spotify…",
                "Spotify에서 가져오기…",
                "Spotifyからインポート…"
            )
            .to_owned(),
        }
    }

    /// Whether the field's value must be hidden when displayed (keys / tokens).
    pub fn is_secret(self) -> bool {
        matches!(self, Field::ApiKey | Field::ListenBrainzToken)
    }
}
