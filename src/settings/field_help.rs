//! Per-[`Field`] help text for the Settings detail line: what the setting does, kept apart from
//! the row's label and value. The match is exhaustive on purpose, so a new field cannot ship
//! without a description.

use crate::t;

use super::{AtlasField, Field};

/// Average render cost of an animation toggle, 1 (lightest) to 5 (heaviest). Mirrors the
/// Graphics-tab section order, which is sorted by the same measure.
pub fn anim_cost(field: Field) -> Option<u8> {
    Some(match field {
        Field::AnimErrorShake
        | Field::AnimLikeBurst
        | Field::AnimTrackIntro
        | Field::AnimSeekFlash
        | Field::AnimPauseFlash
        | Field::AnimVolumeFlash
        | Field::AnimToast => 1,
        Field::AnimAboutFx
        | Field::AnimPopupFade
        | Field::AnimTabs
        | Field::AnimStagger
        | Field::AnimActivity
        | Field::AnimCaret
        | Field::AnimSelection => 2,
        Field::AnimTimeGlow
        | Field::AnimHeart
        | Field::AnimSpinner
        | Field::AnimControls
        | Field::AnimEqBars
        | Field::AnimSeekbar
        | Field::AnimProgressSparkle
        | Field::AnimTitle
        | Field::AnimLyrics
        | Field::AnimBorderChase
        | Field::AnimBorder => 3,
        Field::AnimBounce
        | Field::AnimComets
        | Field::AnimSnow
        | Field::AnimStarfield
        | Field::AnimFireflies
        | Field::AnimCube
        | Field::AnimAquarium
        | Field::AnimWaves
        | Field::AnimVisualizer => 4,
        Field::AnimFireworks
        | Field::AnimRain
        | Field::AnimLife
        | Field::AnimPipes
        | Field::AnimDonut
        | Field::AnimPlasma => 5,
        _ => return None,
    })
}

impl Field {
    /// One or two sentences on what the setting changes. Shown under the list for the focused
    /// row; the label and value stay on the row itself.
    pub fn description(self) -> &'static str {
        match self {
            Field::Atlas(field) => atlas_description(field),
            Field::BeginnerMode => t!(
                "Adds plain-language labels and runs the guided tour on the next launch.",
                "쉬운 설명 라벨을 보여 주고, 다음 실행 때 안내 투어를 시작해요.",
                "わかりやすいラベルを表示し、次回起動時にガイドツアーを始めます。"
            ),
            Field::Language => t!(
                "Language for menus and messages. DJ Gem's reply language is set separately.",
                "메뉴와 메시지의 언어예요. DJ Gem 답변 언어는 따로 설정해요.",
                "メニューとメッセージの言語です。DJ Gemの応答言語は別に設定します。"
            ),
            Field::SearchSource => t!(
                "Which source the search box starts on.",
                "검색창이 처음에 사용할 소스예요.",
                "検索欄が最初に使うソースです。"
            ),
            Field::StreamingSource => t!(
                "Where autoplay and DJ Gem look for the next tracks.",
                "자동재생과 DJ Gem이 다음 곡을 찾는 곳이에요.",
                "自動再生とDJ Gemが次の曲を探す場所です。"
            ),
            Field::SearchYoutube
            | Field::SearchSoundCloud
            | Field::SearchAudius
            | Field::SearchJamendo
            | Field::SearchInternetArchive
            | Field::SearchRadioBrowser => t!(
                "Show this source in the search source list.",
                "검색 소스 목록에 이 소스를 보여 줘요.",
                "検索ソース一覧にこのソースを表示します。"
            ),
            Field::AudiusAppName => t!(
                "App name sent with Audius API requests. Leave empty to use yututui.",
                "Audius API 요청에 붙는 앱 이름이에요. 비워 두면 yututui를 써요.",
                "Audius APIリクエストに付けるアプリ名です。空欄ならyututuiを使います。"
            ),
            Field::JamendoClientId => t!(
                "Your Jamendo API client_id. Jamendo search needs it.",
                "Jamendo API client_id예요. Jamendo 검색에 필요해요.",
                "Jamendo APIのclient_idです。Jamendo検索に必要です。"
            ),
            Field::CookiesFile => t!(
                "Netscape cookies.txt used for YouTube sign-in. Empty uses the default path.",
                "YouTube 로그인에 쓰는 Netscape cookies.txt예요. 비우면 기본 경로를 써요.",
                "YouTubeのサインインに使うNetscape cookies.txtです。空欄なら既定のパスを使います。"
            ),
            Field::DownloadDir => t!(
                "Folder for downloaded tracks. Empty uses the default folder.",
                "다운로드한 곡을 저장할 폴더예요. 비우면 기본 폴더를 써요.",
                "ダウンロードした曲の保存先です。空欄なら既定のフォルダーを使います。"
            ),
            Field::LocalIncludeDownloadDir => t!(
                "Also list the download folder in the local library.",
                "로컬 라이브러리에 다운로드 폴더도 함께 보여 줘요.",
                "ローカルライブラリにダウンロード先も表示します。"
            ),
            Field::LocalMusicRoot => t!(
                "Your music folder for the local library.",
                "로컬 라이브러리가 읽을 음악 폴더예요.",
                "ローカルライブラリが読む音楽フォルダーです。"
            ),
            Field::LocalMusicRootRecursive => t!(
                "Scan folders inside the music folder too.",
                "음악 폴더 안의 하위 폴더까지 읽어요.",
                "音楽フォルダー内のサブフォルダーも読み込みます。"
            ),
            Field::Mouse => t!(
                "Clickable buttons, click-to-seek, and wheel scrolling. Applies after a restart.",
                "버튼 클릭, 클릭 탐색, 휠 스크롤을 켜요. 다시 실행한 뒤 적용돼요.",
                "ボタンのクリック、クリックでのシーク、ホイール操作を有効にします。再起動後に反映されます。"
            ),
            Field::AlbumArt => t!(
                "Draw album art inside the terminal.",
                "터미널 안에 앨범 아트를 그려요.",
                "端末内にアルバムアートを描きます。"
            ),
            Field::PlayerBarPosition => t!(
                "Top keeps the player on the Player screen. Bottom docks it on every screen.",
                "상단은 플레이어 화면에만 두고, 하단은 모든 화면 아래에 고정해요.",
                "上部はプレイヤー画面のみ、下部はすべての画面の下に固定します。"
            ),
            Field::BigText => t!(
                "Enlarge text on terminals that support it, without learning zoom keys.",
                "지원하는 터미널에서 글자를 크게 키워요. 확대 단축키가 필요 없어요.",
                "対応する端末で文字を大きくします。拡大キーを覚える必要はありません。"
            ),
            Field::AutoplayOnStart => t!(
                "Resume the last track as soon as the app opens.",
                "앱을 열면 마지막 곡을 바로 이어서 재생해요.",
                "起動したらすぐ前回の曲を再開します。"
            ),
            Field::EnqueueNext => t!(
                "Added tracks play right after the current one instead of at the end.",
                "추가한 곡을 큐 끝이 아니라 지금 곡 바로 다음에 넣어요.",
                "追加した曲をキューの最後ではなく、今の曲の次に入れます。"
            ),
            Field::UpdateCheck => t!(
                "Ask GitHub for a newer release at startup. Off means no version check.",
                "시작할 때 GitHub에서 새 버전을 확인해요. 끄면 확인하지 않아요.",
                "起動時にGitHubで新しいリリースを確認します。オフなら確認しません。"
            ),
            Field::ExportPersonalData => t!(
                "Save your library, playlists, and preferences as one JSON file. No keys or passwords.",
                "라이브러리, 플레이리스트, 환경설정을 JSON 파일 하나로 저장해요. 키와 비밀번호는 빠져요.",
                "ライブラリ、プレイリスト、設定を1つのJSONに保存します。キーやパスワードは含みません。"
            ),
            Field::ResetKeybindings => t!(
                "Restore every hotkey to its default. Asks first.",
                "모든 핫키를 기본값으로 되돌려요. 실행 전에 확인해요.",
                "すべてのホットキーを既定に戻します。実行前に確認します。"
            ),
            Field::ResetAll => t!(
                "Restore every setting to its default. Asks first.",
                "모든 설정을 기본값으로 되돌려요. 실행 전에 확인해요.",
                "すべての設定を既定に戻します。実行前に確認します。"
            ),
            Field::Speed => t!(
                "How fast tracks play. 1.0x is normal speed.",
                "곡이 재생되는 속도예요. 1.0x가 기본 속도예요.",
                "曲の再生速度です。1.0xが通常の速さです。"
            ),
            Field::SeekInterval => t!(
                "How far one seek key jumps.",
                "탐색 키 한 번에 이동하는 거리예요.",
                "シークキー1回で移動する長さです。"
            ),
            Field::MouseWheelVolume => t!(
                "Scroll over the volume control to change volume.",
                "볼륨 컨트롤 위에서 휠을 굴려 볼륨을 바꿔요.",
                "音量コントロールの上でホイールを回して音量を変えます。"
            ),
            Field::Gapless => t!(
                "No silence between tracks. Applies after a restart.",
                "곡 사이의 무음 없이 이어서 재생해요. 다시 실행한 뒤 적용돼요.",
                "曲間の無音をなくします。再起動後に反映されます。"
            ),
            Field::MediaControls => t!(
                "Show playback in the OS media panel and accept media keys.",
                "OS 미디어 패널에 재생 정보를 보여 주고 미디어 키를 받아요.",
                "OSのメディアパネルに再生情報を出し、メディアキーを受け付けます。"
            ),
            Field::AutoContinueVideos => t!(
                "With the video window open, play the next track's video when one ends.",
                "영상 창이 열려 있으면 곡이 끝날 때 다음 곡 영상을 이어서 재생해요.",
                "動画ウィンドウが開いていれば、曲の終わりに次の曲の動画を再生します。"
            ),
            Field::VideoLayout => t!(
                "Where the video window opens. Shift+V switches it while it is open.",
                "영상 창이 열리는 위치예요. 열린 동안에는 Shift+V로 바꿔요.",
                "動画ウィンドウの開く位置です。開いている間はShift+Vで切り替えます。"
            ),
            Field::AlbumArtQuality => t!(
                "Detail level for online album art. Higher loads larger images.",
                "온라인 앨범 아트의 화질이에요. 높을수록 큰 이미지를 불러와요.",
                "オンラインのアルバムアートの画質です。高いほど大きな画像を読み込みます。"
            ),
            Field::LocalCrossfade => t!(
                "Blend the end of one local file into the next.",
                "로컬 파일끼리 앞 곡의 끝과 다음 곡의 시작을 겹쳐요.",
                "ローカルファイル同士で曲の終わりと次の曲の始まりを重ねます。"
            ),
            Field::RadioRecording => t!(
                "Opens radio recording options.",
                "라디오 녹음 옵션을 열어요.",
                "ラジオ録音のオプションを開きます。"
            ),
            Field::AudioBackend => t!(
                "The audio engine. mpv is the only one available.",
                "오디오 엔진이에요. 지금은 mpv만 쓸 수 있어요.",
                "オーディオエンジンです。現在はmpvのみです。"
            ),
            Field::AudioOutput => t!(
                "Opens a list of detected speakers and headphones.",
                "감지된 스피커와 헤드폰 목록을 열어요.",
                "検出されたスピーカーとヘッドホンの一覧を開きます。"
            ),
            Field::AudioMpvOutput => t!(
                "mpv audio output driver. Empty or auto uses mpv's default.",
                "mpv 오디오 출력 드라이버예요. 비우거나 auto면 mpv 기본값을 써요.",
                "mpvの音声出力ドライバーです。空欄かautoならmpvの既定です。"
            ),
            Field::AudioMpvDevice => t!(
                "mpv audio device. Empty or auto uses mpv's default.",
                "mpv 오디오 장치예요. 비우거나 auto면 mpv 기본값을 써요.",
                "mpvの音声デバイスです。空欄かautoならmpvの既定です。"
            ),
            Field::LongFormSeekOptimization => t!(
                "Faster seeking in long videos and mixes. Auto is experimental.",
                "긴 영상과 믹스에서 탐색을 빠르게 해요. 자동은 실험 기능이에요.",
                "長い動画やミックスのシークを速くします。自動は実験的な機能です。"
            ),
            Field::AudioMpvCacheForward => t!(
                "How much mpv buffers ahead. Applies after a restart.",
                "mpv가 앞쪽으로 미리 받아 두는 양이에요. 다시 실행한 뒤 적용돼요.",
                "mpvが先読みする量です。再起動後に反映されます。"
            ),
            Field::AudioMpvCacheBack => t!(
                "How much mpv keeps behind the playhead. Applies after a restart.",
                "mpv가 재생 위치 뒤쪽에 남겨 두는 양이에요. 다시 실행한 뒤 적용돼요.",
                "mpvが再生位置の後ろに残す量です。再起動後に反映されます。"
            ),
            Field::EqPreset => t!(
                "A starting curve for the ten bands below. Moving a band makes it custom.",
                "아래 10개 밴드의 기본 곡선이에요. 밴드를 움직이면 사용자 설정이 돼요.",
                "下の10バンドの基本カーブです。バンドを動かすとカスタムになります。"
            ),
            Field::Band(_) => t!(
                "Boost or cut this frequency band.",
                "이 주파수 대역을 키우거나 줄여요.",
                "この周波数帯を強めたり弱めたりします。"
            ),
            Field::Normalize => t!(
                "Even out loud and quiet tracks.",
                "큰 곡과 작은 곡의 음량 차이를 줄여요.",
                "大きい曲と小さい曲の音量差を減らします。"
            ),
            Field::AiEnabled => t!(
                "Turns DJ Gem on or off. The API key stays saved.",
                "DJ Gem을 켜고 꺼요. API 키는 그대로 저장돼요.",
                "DJ Gemのオン/オフです。APIキーは保存されたままです。"
            ),
            Field::GeminiModel => t!(
                "The Gemini model DJ Gem uses.",
                "DJ Gem이 쓰는 Gemini 모델이에요.",
                "DJ Gemが使うGeminiモデルです。"
            ),
            Field::ApiKey => t!(
                "Your Gemini API key. It is never shown after saving.",
                "Gemini API 키예요. 저장한 뒤에는 화면에 보이지 않아요.",
                "Gemini APIキーです。保存後は表示されません。"
            ),
            Field::DjGemLanguage => t!(
                "The language DJ Gem answers in. Retro mode always uses English.",
                "DJ Gem이 답하는 언어예요. 레트로 모드에서는 항상 영어예요.",
                "DJ Gemが答える言語です。レトロモードでは常に英語です。"
            ),
            Field::RomanizedTitles => t!(
                "Show Korean, Japanese, and Chinese titles in Latin letters. Source data is unchanged.",
                "한국어·일본어·중국어 제목을 로마자로 보여 줘요. 원본 정보는 바뀌지 않아요.",
                "韓国語・日本語・中国語のタイトルをローマ字で表示します。元の情報は変わりません。"
            ),
            Field::ClearRomanizedTitleCache => t!(
                "Delete saved romanized titles so they are made again.",
                "저장된 로마자 제목을 지워서 다시 만들게 해요.",
                "保存済みのローマ字タイトルを消して作り直します。"
            ),
            Field::AutoplayStreaming => t!(
                "Keep playing similar tracks when the queue runs out.",
                "큐가 끝나면 비슷한 곡을 계속 재생해요.",
                "キューが終わったら似た曲を再生し続けます。"
            ),
            Field::CuratingMode => t!(
                "YouTube-native picks locally. DJ Gem also reorders the picks with AI.",
                "YouTube 기본은 로컬에서만 고르고, DJ Gem은 AI로 순서를 한 번 더 정해요.",
                "YouTubeネイティブはローカルで選曲し、DJ GemはAIで順番をさらに調整します。"
            ),
            Field::StreamingMode => t!(
                "Focused stays close to the seed. Discovery explores further.",
                "집중은 처음 곡에 가깝게, 탐색은 더 넓게 골라요.",
                "フォーカスは元の曲に近く、ディスカバリーはより広く選びます。"
            ),
            Field::RetroMode => t!(
                "For the Linux text console: English UI, Retro theme, and plain ASCII drawing.",
                "리눅스 텍스트 콘솔용이에요. 영어 UI, 레트로 테마, ASCII 그리기를 써요.",
                "Linuxテキストコンソール向けです。英語UI、レトロテーマ、ASCII描画を使います。"
            ),
            Field::ThemePreset => t!(
                "A complete color theme. Color edits below apply on top of it.",
                "전체 색상 테마예요. 아래에서 바꾼 색은 이 테마 위에 덮어써요.",
                "配色テーマ一式です。下で変えた色はこのテーマに上書きされます。"
            ),
            Field::BackgroundNone => t!(
                "Let the terminal's own background show through.",
                "터미널 자체 배경이 비쳐 보이게 해요.",
                "端末自体の背景を透かして表示します。"
            ),
            Field::ThemeColor(_) => t!(
                "Click the swatch for the palette, or press Enter to type #RRGGBB. none is transparent.",
                "견본을 클릭하면 팔레트가 열리고, Enter로 #RRGGBB를 입력해요. none은 투명이에요.",
                "見本をクリックするとパレット、Enterで#RRGGBBを入力します。noneは透明です。"
            ),
            Field::AnimMaster => t!(
                "Master switch. Off stops every effect below.",
                "전체 스위치예요. 끄면 아래 효과가 모두 멈춰요.",
                "全体スイッチです。オフにすると下の効果がすべて止まります。"
            ),
            Field::AnimFps => t!(
                "Animation frame rate. Above 30 fps uses noticeably more CPU.",
                "애니메이션 프레임 레이트예요. 30fps를 넘기면 CPU를 눈에 띄게 더 써요.",
                "アニメーションのフレームレートです。30fpsを超えるとCPU負荷が目立って増えます。"
            ),
            Field::AnimPauseUnfocused => t!(
                "Stop animating while the terminal window is in the background.",
                "터미널 창이 뒤에 있을 때 애니메이션을 멈춰요.",
                "端末ウィンドウが背面にある間はアニメーションを止めます。"
            ),
            Field::AnimErrorShake
            | Field::AnimLikeBurst
            | Field::AnimTrackIntro
            | Field::AnimSeekFlash
            | Field::AnimPauseFlash
            | Field::AnimVolumeFlash
            | Field::AnimToast => t!(
                "A short effect that plays once when this event happens.",
                "해당 이벤트가 일어날 때 한 번만 짧게 재생돼요.",
                "そのイベントが起きたときに一度だけ短く再生されます。"
            ),
            Field::AnimAboutFx
            | Field::AnimPopupFade
            | Field::AnimTabs
            | Field::AnimStagger
            | Field::AnimActivity
            | Field::AnimCaret
            | Field::AnimSelection => t!(
                "Motion on lists, tabs, popups, and the text cursor across the app.",
                "앱 전체의 목록, 탭, 팝업, 입력 커서에 움직임을 줘요.",
                "アプリ全体のリスト、タブ、ポップアップ、カーソルに動きを付けます。"
            ),
            Field::AnimTimeGlow
            | Field::AnimHeart
            | Field::AnimSpinner
            | Field::AnimControls
            | Field::AnimEqBars
            | Field::AnimSeekbar
            | Field::AnimProgressSparkle
            | Field::AnimTitle
            | Field::AnimLyrics
            | Field::AnimBorderChase
            | Field::AnimBorder => t!(
                "Runs on the player while a track plays.",
                "곡이 재생되는 동안 플레이어에서 계속 움직여요.",
                "曲の再生中、プレイヤー上で動き続けます。"
            ),
            Field::AnimBounce
            | Field::AnimComets
            | Field::AnimSnow
            | Field::AnimStarfield
            | Field::AnimFireflies
            | Field::AnimCube
            | Field::AnimAquarium
            | Field::AnimWaves
            | Field::AnimVisualizer => t!(
                "Fills empty player space with a background scene.",
                "플레이어의 빈 공간을 배경 장면으로 채워요.",
                "プレイヤーの空きスペースを背景シーンで埋めます。"
            ),
            Field::AnimFireworks
            | Field::AnimRain
            | Field::AnimLife
            | Field::AnimPipes
            | Field::AnimDonut
            | Field::AnimPlasma => t!(
                "A full background scene for empty player space. The heaviest effects.",
                "플레이어의 빈 공간을 채우는 큰 배경 장면이에요. 가장 무거운 효과예요.",
                "プレイヤーの空きスペースを埋める大きな背景シーンです。最も重い効果です。"
            ),
            Field::LastfmEnabled => t!(
                "Send finished tracks to your Last.fm profile.",
                "다 들은 곡을 Last.fm 프로필에 기록해요.",
                "聴き終えた曲をLast.fmのプロフィールに記録します。"
            ),
            Field::LastfmConnect => t!(
                "Sign in through the browser, or disconnect this account.",
                "브라우저로 로그인하거나 이 계정 연결을 끊어요.",
                "ブラウザでサインインするか、このアカウントの連携を解除します。"
            ),
            Field::LastfmLoveSync => t!(
                "Liking a track here also loves it on Last.fm.",
                "여기서 좋아요를 누르면 Last.fm에서도 love로 표시해요.",
                "ここで高評価するとLast.fmでもloveにします。"
            ),
            Field::ListenBrainzEnabled => t!(
                "Send finished tracks to ListenBrainz.",
                "다 들은 곡을 ListenBrainz에 기록해요.",
                "聴き終えた曲をListenBrainzに記録します。"
            ),
            Field::ListenBrainzToken => t!(
                "Your user token from listenbrainz.org/settings.",
                "listenbrainz.org/settings에서 받은 사용자 토큰이에요.",
                "listenbrainz.org/settingsで取得したユーザートークンです。"
            ),
            Field::ScrobbleLocalFiles => t!(
                "Also scrobble local files that have a title and artist.",
                "제목과 아티스트가 있는 로컬 파일도 기록해요.",
                "タイトルとアーティストがあるローカル曲も記録します。"
            ),
            Field::SpotifyClientId => t!(
                "Client ID of your own app from developer.spotify.com.",
                "developer.spotify.com에서 만든 내 앱의 클라이언트 ID예요.",
                "developer.spotify.comで作った自分のアプリのクライアントIDです。"
            ),
            Field::SpotifyRedirectPort => t!(
                "Local port for sign-in. It must match the redirect URI in your Spotify app.",
                "로그인에 쓰는 로컬 포트예요. Spotify 앱의 리다이렉트 URI와 같아야 해요.",
                "サインイン用のローカルポートです。Spotifyアプリのリダイレクトと一致させます。"
            ),
            Field::SpotifyConnect => t!(
                "Sign in through the browser, or disconnect this account.",
                "브라우저로 로그인하거나 이 계정 연결을 끊어요.",
                "ブラウザでサインインするか、このアカウントの連携を解除します。"
            ),
            Field::SpotifyImportMode => t!(
                "How imported Spotify playlists are written to your Library.",
                "가져온 Spotify 플레이리스트를 라이브러리에 저장하는 방식이에요.",
                "取り込んだSpotifyプレイリストをライブラリに書き込む方法です。"
            ),
            Field::SpotifyImport => t!(
                "Pick one of your Spotify playlists to import.",
                "가져올 Spotify 플레이리스트를 골라요.",
                "取り込むSpotifyプレイリストを選びます。"
            ),
        }
    }
}

fn atlas_description(field: AtlasField) -> &'static str {
    match field {
        AtlasField::Renderer => t!(
            "How the globe is drawn. Auto uses Braille dots, or ASCII in retro mode.",
            "지구본을 그리는 방식이에요. 자동은 점자 도트, 레트로 모드에서는 ASCII예요.",
            "地球儀の描き方です。自動は点字ドット、レトロモードではASCIIです。"
        ),
        AtlasField::StationLimit => t!(
            "How many stations to load. More costs more network and cache, not drawing.",
            "불러올 방송국 수예요. 많을수록 네트워크와 캐시를 더 쓰고, 그리기 비용은 같아요.",
            "読み込む局の数です。多いほど通信とキャッシュを使い、描画負荷は変わりません。"
        ),
        AtlasField::Panel => t!(
            "The station list beside the globe. Auto shows it on wide terminals.",
            "지구본 옆 방송국 목록이에요. 자동은 넓은 터미널에서만 보여요.",
            "地球儀の横の局リストです。自動は広い端末でのみ表示します。"
        ),
        AtlasField::Coast => t!(
            "Keep spinning briefly after a drag. Needs animations on.",
            "드래그한 뒤 잠시 더 회전해요. 애니메이션이 켜져 있어야 해요.",
            "ドラッグ後もしばらく回ります。アニメーションが必要です。"
        ),
        AtlasField::Grid => t!(
            "Draw latitude and longitude lines every 30°.",
            "30°마다 위도·경도 선을 그려요.",
            "30°ごとに緯線と経線を描きます。"
        ),
        AtlasField::FollowPlaying => t!(
            "Turn the globe to a station when it starts playing.",
            "방송국 재생이 시작되면 지구본을 그쪽으로 돌려요.",
            "局の再生が始まると地球儀をその位置に回します。"
        ),
        AtlasField::Autorotate => t!(
            "Spin slowly while idle. Needs animations on.",
            "가만히 있을 때 천천히 회전해요. 애니메이션이 켜져 있어야 해요.",
            "操作していない間ゆっくり回ります。アニメーションが必要です。"
        ),
    }
}
