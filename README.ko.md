# ytm-tui

[English](README.md) · **한국어** · [日本語](README.ja.md)

[![Release](https://img.shields.io/github/v/release/Ochichan/ytm-tui)](https://github.com/Ochichan/ytm-tui/releases)
[![License: MIT](https://img.shields.io/badge/license-MIT-8aadf4.svg)](LICENSE)

### [▶ 라이브 데모 · 기능 둘러보기 → ochichan.github.io/ytm-tui](https://ochichan.github.io/ytm-tui/)

터미널 안에서 즐기는 YouTube Music. 빠르고, 키보드로 다루고, 램을 야금야금 먹는 브라우저 탭도 광고도 없습니다.
DJ Gem 스트리밍, 싱크 가사가 흐르는 진짜 앨범 아트, Last.fm / ListenBrainz 스크로블링, 명령 한 줄로 끝나는
Spotify 이사, 어디서든 통하는 원격 제어까지 — 전부 세 글자 명령 하나로: `ytt`.

Rust + ratatui. MIT.

---

## 설치

각 명령은 `ytt`와 보조 프로그램(mpv, yt-dlp, ffmpeg)을 **한 번에** 함께 설치합니다.

| OS | 한 줄이면 끝 |
| --- | --- |
| **macOS** | `brew install Ochichan/tap/ytm-tui` |
| **Windows** | `scoop bucket add extras; scoop bucket add ytm-tui https://github.com/Ochichan/scoop-bucket; scoop install ytm-tui` |
| **Linux** — 아무 배포판, [Nix](https://nixos.org/download) | `nix run github:Ochichan/ytm-tui` |
| **Linux** — Arch | `yay -S ytm-tui-bin` |
| **Linux** — 그 외 | 아래 설치 스크립트 실행 |
| **소스에서 빌드** | `./install.sh --build` ([Rust](https://rustup.rs) 필요) |

```sh
curl -fsSL https://raw.githubusercontent.com/Ochichan/ytm-tui/main/install.sh | bash
```

> `curl | bash`와 소스 빌드 방식은 `ytt`만 설치합니다. 보조 프로그램은 직접 설치하거나(`brew install mpv yt-dlp ffmpeg`, `sudo apt install mpv yt-dlp ffmpeg`, `sudo pacman -S mpv yt-dlp ffmpeg`) — 설치 후 `ytt doctor`로 뭐가 빠졌는지 확인하세요.
> Windows에서는 Scoop이 `ytt-tray.exe`와 **YtmTui Tray** 바로 가기도 설치합니다 — 알림 영역의 미니 플레이어예요([아래 상세](#미디어-키--os-통합)). 터미널에서 실행 중인 `ytt` 세션의 작업 표시줄 버튼은 여전히 Windows Terminal에 속합니다.
> macOS에서도 Homebrew와 릴리스 아카이브에 `ytt-tray`가 함께 들어갑니다 — 같은 동반 앱이 메뉴 바에 상주해요(v1.5.8 이후 릴리스).
> 트레이 시작 프로그램 등록은 양쪽 모두 선택 사항입니다. 켜려면 `ytt-tray --install-startup`, 제거하려면 `ytt-tray --uninstall-startup`.
> 백그라운드 재생: `ytt daemon start --resume`으로 저장된 큐를 headless 음악 데몬에서 시작하고, `ytt -r status`, `ytt -r pp`, `ytt -r next`, `ytt -r play "lofi"`, `ytt daemon stop`으로 제어하세요.

---

## 빠른 시작

```sh
ytt
```

1. **`s`** 를 누르고, 곡 제목을 입력한 뒤 **`Enter`**.
2. **`↑`/`↓`** 로 이동하고 **`Enter`** 로 재생.
3. 언제든 **`?`** 를 누르면 항상 최신인 전체 키 목록이 나옵니다.

끝. 음악 나옵니다.

> **뭔가 이상한가요?** **`ytt doctor`** 를 실행하세요 — mpv, yt-dlp, ffmpeg를 점검하고 정확히 뭘 고쳐야 할지 알려줍니다. `ytt: command not found`가 뜬다면? 새 터미널 창을 열어 `PATH`가 따라오게 하세요.

---

## 이런 걸 합니다

- **DJ Gem 스트리밍** — **`Ctrl+R`** 을 누르면 지금 듣는 곡을 중심으로 끝없는 스테이션을 만들어줍니다. 세 가지 무드: Focused, Balanced, Discovery. **`w`** 를 누르면 각 곡을 고른 이유를 쉬운 말로 보여줘요.
- **카탈로그 여섯, 검색창 하나** — 본진은 YouTube Music이지만, 검색에서 **`Tab`** 을 누르면 SoundCloud, Audius, Jamendo, Internet Archive, Radio Browser로 전환됩니다(전부 한꺼번에도 가능, 결과마다 `[SRC]` 태그). 앱 전체를 인터넷 라디오 튜너로 바꾸는 전용 라디오 모드(**`Alt+Shift+R`**)도 있어요 — 라디오만의 좋아요와 히스토리 포함.
- **진짜 앨범 아트 + 싱크 가사 + 뮤직비디오** — 실제 커버 이미지가 플레이어에 그대로 그려집니다(Kitty/Sixel/iTerm2 그래픽 자동 감지). 그 아래로 시간 싱크 가사가 흐르고(**`Shift+L`**), 듣는 것만으로 아쉬우면 **`v`** — 뮤직비디오가 터미널 위 작은 mpv 창에 뜹니다. *영상 자동 이어재생*(설정 → 재생)을 켜면 영상이 끝날 때 큐의 다음 곡 뮤직비디오가 같은 창에서 바로 이어집니다.
- **검색 · 라이브러리 · 큐 · 플레이리스트** — **`s`** 검색, **`l`** 라이브러리(즐겨찾기·기록·다운로드·플레이리스트), **`c`** 큐. 앱 안에서 플레이리스트를 만들거나(**`n`**), DJ Gem에게 만들어 달라고 하세요.
- **다운로드** — **`d`** 를 누르면 커버 아트와 태그가 박힌 m4a로 저장되고, 다운로드 탭에서 오프라인 재생됩니다.
- **스크로블링** — Last.fm과 ListenBrainz, 크래시에도 안전한 오프라인 큐와 좋아요→love 동기화까지. [아래 상세](#스크로블링-lastfm--listenbrainz).
- **Spotify 가져오기 / 내보내기** — 좋아요와 플레이리스트가 체크포인트·이어하기가 되는 명령 한 줄로 이사 옵니다. [아래 상세](#spotify-가져오기--내보내기).
- **어디서든 제어** — 미디어 키, macOS Control Center, Windows SMTC + 트레이 미니 플레이어, Linux MPRIS, 아무 셸에서나 `ytt -r`, 아예 터미널 없는 headless 데몬까지. [아래 상세](#미디어-키--os-통합).
- **내 마음대로** — 테마 13종, 색 역할 34개 전부 hex로 편집, 모든 키 재설정, 프리셋을 갖춘 10밴드 EQ, 고요한 정지 화면부터 빙글빙글 도는 ASCII 도넛까지의 애니메이션 — 그리고 맨몸 리눅스 콘솔에서도 도는 레트로 모드(ASCII 앨범 아트 포함).
- **DJ Gem 어시스턴트** *(선택)* — **`g`** 를 누르고 말로 시키세요: *"lo-fi 틀어줘", "신나는 곡 3개 큐에 넣어줘", "비 오는 날 플레이리스트 만들어줘"*. 무료 Google Gemini 키가 필요하고, 나머지 기능은 키 없이도 전부 동작합니다.

앱 인터페이스는 **English와 한국어**를 지원합니다(설정 → 일반 → 언어). 이 README는 [English](README.md)와 [日本語](README.ja.md)로도 있습니다.

---

## 필수 키

앱에서 **`?`** 를 누르면 완전한 라이브 치트시트가 나옵니다 — *내가 바꾼* 키 그대로 반영되고, 아래 모든 키는 재설정할 수 있어요(설정 → 핫키). 기본만 추리면:

| 키 | 동작 |
| --- | --- |
| `Space` | 재생 / 일시정지 |
| `←` / `→` | 뒤로 / 앞으로 탐색 |
| `↑` / `↓` | 볼륨 업 / 다운 |
| `m` | 음소거 / 해제 |
| `,` / `.` | 이전 / 다음 |
| `s` | 검색 (`Tab` 으로 소스 선택) |
| `l` | 라이브러리 · `a` 탭 전체 재생 · `\` 큐에 추가 · `/` 필터 |
| `c` | 큐 |
| `f` | 즐겨찾기 / 평가 |
| `d` | 다운로드 |
| `P` | 플레이리스트에 추가 (목록에서는 `p`) |
| `Shift+L` | 싱크 가사 |
| `v` | 뮤직비디오 오버레이 (`V` 로 위치 전환) |
| `y` | 곡 링크 복사 |
| `Ctrl+R` | DJ Gem 스트리밍 켜기/끄기 |
| `w` | DJ Gem 선곡 이유 |
| `g` | DJ Gem 어시스턴트 |
| `Shift+S` / `r` | 셔플 / 반복 전환 |
| `e` | EQ 프리셋 (Flat · Bass · Treble · Vocal · Rock · Jazz) |
| `[` / `]` | 재생 속도 (0.5×–2×) |
| `Ctrl+-` / `Ctrl+=` | 글자 축소 / 확대 (Ctrl+휠 또는 설정 → 큰 글자 모드, kitty·Windows Terminal 등) |
| `o` | 설정 |
| `Ctrl+Q` | 종료 |

> **한글 자판이세요?** 단축키가 두벌식 자모를 알아듣습니다. `ㅂ` 은 `q` 처럼, `ㄱ` 은 `r` 처럼 — 입력기를 바꿀 필요가 없어요. 마우스가 편하면 화면의 모든 것이 클릭되고, 휠은 볼륨을 탑니다.

---

## 플레이리스트

라이브러리의 **플레이리스트** 탭은 나만의 로컬 플레이리스트를 담습니다 — 앱 안에서 만들고(**`n`**), 어디서든 채우고(재생 중 곡은 **`P`**, 목록 행에서는 **`p`**), 마음대로 정리하고, 통째로 재생하거나 큐에 넣습니다(**`a`** / **`\`**). 설정 파일 옆의 평범한 `playlists.json`에 저장되니 백업도 그냥 파일 복사예요.

Spotify 가져오기를 여기로 받을 수도 있고(`--to local`), DJ Gem 어시스턴트가 요청대로 만들고 채워줄 수도 있으며, `ytt transfer export local:<이름> --to spotify` 로 반대 방향도 됩니다.

---

## 원격 제어 & 데몬

`ytt` 가 재생 중이면 다른 터미널에서 — 또는 미디어 키로 — `ytt -r` 로 제어할 수 있습니다:

```sh
ytt -r pp                  # 재생 / 일시정지   (별칭: toggle, play, pause)
ytt -r next / prev         # 곡 이동
ytt -r volume 40           # 볼륨 지정; up / down 도 가능
ytt -r back / fwd          # 설정된 간격만큼 탐색
ytt -r seek-to 90          # 1:30 지점으로 점프
ytt -r streaming on        # 무한 스트리밍: on / off / toggle
ytt -r play "lofi"         # 데몬: 검색해서 첫 결과 재생
ytt -r enqueue "city pop"  # 데몬: 검색해서 첫 결과를 큐에 추가
ytt -r status              # 한 줄 "지금 재생 중" (--json 스크립트용, -q 조용히)
ytt -r quit                # 정지하고 종료
```

미디어 키에 연결(i3 / sway):

```
bindsym XF86AudioPlay exec ytt -r pp
bindsym XF86AudioNext exec ytt -r next
```

터미널 없는 재생은 데몬으로:

```sh
ytt daemon start --resume   # 저장된 큐/세션을 복원해 재생
ytt daemon status --json    # 소유자/상태 스냅샷 (스크립트용)
ytt daemon stop             # 데몬 중지 + mpv 정리
```

데몬 resume은 저장된 큐 순서, 커서, 셔플/반복, 일반/라디오 모드 큐를 복원합니다. 자동 스트리밍은 TUI와 같은 추천 경로로 headless 상태에서도 큐를 계속 채우고, 스크로블링도 OS 미디어 컨트롤도 그대로 동작해요. `ytt -r play …` / `ytt -r enqueue …` 는 데몬 전용 검색 명령이라 단독 TUI는 거절합니다.

`ytt` 를 두 번 실행해도 스피커를 두고 싸우는 두 번째 플레이어가 생기지 않습니다 — 이미 있는 쪽을 조종하는 방법만 알려줘요(정말 두 개가 필요하면 `ytt --new-instance`). 전체 명령은 `ytt -r --help` 와 `ytt daemon --help` 로.

---

## 미디어 키 & OS 통합

`ytt` 는 OS가 음악을 보여주는 모든 곳에 나타납니다 — 기본으로 켜져 있고, 설정 → 재생 → *OS 미디어 컨트롤*에서 끌 수 있어요. TUI든 데몬이든 똑같이 동작합니다.

- **macOS** — Control Center의 진짜 Now Playing 카드: 앨범 아트, 실제로 움직이는 시크바, 다음/이전, 좋아요 버튼까지. AirPods 꼭지 클릭도 기대한 대로 동작합니다. 그리고 **YtmTui Tray** 동반 앱이 메뉴 바에 상주해요(`ytt-tray`, brew와 릴리스 아카이브에 포함) — Windows와 같은 미니 플레이어와 메뉴가 시계 옆 한 클릭 거리에.
- **Windows** — 앨범 아트와 시크가 있는 SMTC 미디어 오버레이, 그리고 선택형 **YtmTui Tray** 동반 앱(Scoop이 설치): 왼쪽 클릭이면 Now / Queue / Stream / Tune 탭의 미니 플레이어, 오른쪽 클릭이면 전체 메뉴 — 데몬 시작/중지, 마지막 세션 이어듣기, TUI 열기. 로그인 시 자동 실행은 `ytt-tray --install-startup`(선택 사항). 설치 스크립트가 앱 아이덴티티도 등록해 플라이아웃에 **YtmTui**로 표시됩니다 — 혹시 "알 수 없는 앱"으로 나오면 `ytt register-media-identity`를 한 번 실행하세요.
- **Linux** — 당당한 MPRIS 플레이어(`org.mpris.MediaPlayer2.ytmtui`): playerctl, GNOME/KDE 미디어 위젯, waybar가 전부 그냥 인식합니다.

---

## 스크로블링 (Last.fm / ListenBrainz)

`ytt` 는 실제로 들은 것만 스크로블합니다 — Now Playing 갱신, 표준 하프트랙/4분 규칙,
앱 내 좋아요의 love/unlove 동기화, 그리고 튼튼한 오프라인 큐(비행기에서 들은 곡은 온라인이
되면 배달됩니다 — 네트워크를 시도하기 *전에* 디스크에 먼저 적히므로 크래시에도 잃지 않아요).
TUI와 headless 데몬 모두에서, OS 미디어 컨트롤 설정과는 독립적으로 동작합니다.

- **Last.fm** — 설정 → **계정** → *Last.fm account* → 브라우저에서 승인. 또는
  headless로: `ytt auth lastfm`. 내장 API 자격 증명이 없는 자가 빌드 바이너리는
  `config.json`의 `scrobble.lastfm.api_key` / `api_secret`으로 직접 설정할 수 있어요
  ([API 계정 만들기](https://www.last.fm/api/account/create)).
- **ListenBrainz** — [사용자 토큰](https://listenbrainz.org/settings/)을
  설정 → 계정에 붙여넣거나 `ytt auth listenbrainz <token>` 실행. 셀프 호스팅 인스턴스는
  `scrobble.listenbrainz.api_url`을 설정하세요.
- 서비스별 토글, 좋아요→love 동기화, 로컬 파일 스크로블링이 같은 탭에 있습니다.
  아직 배달 안 된 감상 기록은 설정 파일 옆 `scrobble-queue.jsonl`에서 대기해요.

## Spotify 가져오기 / 내보내기

Spotify, YouTube Music, 일반 파일 사이에서 플레이리스트를 옮기세요 — 체크포인트와
이어하기가 되는 작업, 애매한 곡은 매치 리포트로:

```sh
ytt auth spotify --client-id <YOUR-CLIENT-ID>   # 최초 1회 PKCE 브라우저 연결
ytt transfer list spotify                        # 플레이리스트 id 찾기 (list ytm 도 가능)
ytt transfer import <spotify-url-or-id>          # → 새 YTM 플레이리스트
ytt transfer import liked --to likes             # Spotify 좋아요 → YTM 좋아요 (순서 유지)
ytt transfer import <id> --to local:"운동"        # → 앱 라이브러리 플레이리스트로
ytt transfer import backup.csv --to-playlist "복원"   # Exportify CSV / ytm-tui JSON
ytt transfer export ytm:<id> --to spotify        # 반대 방향
ytt transfer backup --dir ~/music-backup --csv   # 모든 YTM 플레이리스트 → JSON (+CSV)
ytt transfer resume <job-id>                     # 레이트 리밋/중단 후 이어하기
```

TUI 안에서도 됩니다: 설정 → **계정** → *Import from Spotify…* 로 플레이리스트를 고르면
음악을 계속 들으면서 라이브러리의 플레이리스트 탭으로 가져와요. 진행 상황은 상태 줄에 흐릅니다.

**Spotify 설정 (최초 1회).** Development Mode의 Spotify 앱은 자기 허용 목록에 있는 계정만
받아주므로, 본인의 (무료) 앱을 하나 만듭니다:
[developer.spotify.com/dashboard](https://developer.spotify.com/dashboard)에서 앱을 만들고,
리디렉트 URI로 `http://127.0.0.1:9271/callback`을 **정확히** 추가하고(루프백 IP, `localhost`
아님; 9271이 사용 중이면 `spotify.redirect_port`로 변경), *User Management*에 본인 Spotify
계정을 추가한 뒤, 앱의 Client ID를 설정 → 계정에 붙여넣으세요(또는
`ytt auth spotify --client-id …`). 클라이언트 시크릿은 없습니다 — PKCE 흐름은 시크릿을 안 써요.

매칭은 메타데이터 기반입니다(NFKC 정규화, CJK 안전한 제목 + 아티스트 + 길이 + 앨범
타이브레이크). 허용 점수 미만은 조용히 때려 맞추는 대신 작업 리포트에 *ambiguous* 또는
*not found*로 남습니다. `--take-best`, `--min-score`로 다시 돌리거나 손으로 고치세요.
큰 플레이리스트는 `--dry-run`으로 돌려 확인한 뒤 `ytt transfer resume <job-id>`로 쓰면 됩니다.

---

## 내 마음대로 꾸미기

- **테마** — 내장 13종(Default, Midnight, Light, High Contrast, Terminal Green, Gruvbox, Nord, Dracula, Tokyo Night, Solarized Dark, Rosé Pine, Dario, Retro), 그리고 34개 색 역할 전부가 설정 → 그래픽에서 `#RRGGBB`(투명은 `none`)를 받습니다.
- **EQ** — 설정 → 재생의 진짜 10밴드 그래픽 EQ(31 Hz–16 kHz); **`e`** 가 프리셋을 순환하고 **`N`** 이 라우드니스 노멀라이즈를 토글합니다.
- **애니메이션** — 내비게이션의 `✨`(또는 **`A`**)로 전체 토글; 설정 → 그래픽에서 25종 중 골라요: 제목 반짝임, 뛰는 하트, 시크바 글로우, EQ 바, 곡 시작 타이틀 등장, 가사 글로우, 타자기 상태 메시지, 볼륨·탐색·좋아요 피드백 플래시, 숨쉬는 선택 줄, 목록 캐스케이드, 깜빡이는 커서, 탭 팝, 팝업 페이드인, 진행 표시 점, 정보 카드 반짝임, 매트릭스 레인, 별밭, DVD 스타일 튀는 로고, 그리고 네, 빙글빙글 도는 ASCII 도넛.
- **레트로 모드** — 토글 하나(설정 → 그래픽)로 모든 것이 CP437 안전이 됩니다. 맨몸 리눅스 콘솔이나 낡은 SSH 세션용: Retro 테마, ASCII 전용 글리프, 그리고 앨범 아트를 정직한 ASCII 아트로 다시 그려줘요.
- **키 & 마우스** — 모든 바인딩이 충돌 감지와 함께 재설정 가능하고(설정 → 핫키), 클릭파를 위해 UI 전체가 마우스를 압니다.

---

## 문제 해결

| 증상 | 해결 |
| --- | --- |
| 아무것도 재생되지 않거나 재생 시 오류 | mpv 또는 yt-dlp가 없거나 `PATH`에 없습니다. `ytt doctor`를 실행하세요. |
| `ytt: command not found` | 새 터미널을 여세요. 그래도 안 되면 설치기가 출력한 `PATH` 줄을 추가하세요. |
| 어제는 됐는데 오늘은 안 됨 | YouTube가 뭔가 바꿨어요 — yt-dlp를 업데이트하세요(`brew upgrade yt-dlp`, `scoop update yt-dlp`, 또는 패키지 매니저). |
| 특정 곡만 재생 안 됨 | 로그인이 필요할 수 있어요 — 아래 쿠키 참고. |
| 앨범 아트가 안 보임 | 기본은 꺼짐이고 터미널을 탑니다. **앨범 아트**(설정 → 일반)를 켜고 재시작하세요. |
| `v` 를 눌러도 영상이 안 뜸 | 별도 mpv 창을 띄우는 기능이에요 — `ytt doctor`로 mpv를 확인하세요. 로컬 전용 트랙은 보여줄 영상이 없습니다. |
| Control Center / SMTC / MPRIS에 안 나옴 | 설정 → 재생 → **OS 미디어 컨트롤**을 확인하세요. 뭔가 한 번 재생된 뒤부터 표시됩니다. |
| 미디어 플라이아웃에 "알 수 없는 앱" / 항목이 2개 | `ytt register-media-identity`를 한 번 실행하세요(설치 스크립트는 자동으로 합니다). 항목이 2개면 mpv 자체 미디어 세션이 켜진 것 — mpv ≥ 0.39에서는 `ytt`가 자동으로 꺼줍니다. 일부러 켜려면 `YTM_MPV_EXTRA=--media-controls=yes`. |
| DJ Gem이 응답 안 함 | 설정 → DJ Gem에 무료 Gemini 키를 넣고 **Enable DJ Gem**을 켜세요. |
| Spotify 연결/가져오기에서 403 | 앱이 Development Mode입니다. 개발자 대시보드의 *User Management*에 본인 Spotify 계정을 추가하고 Client ID를 다시 확인하세요. |
| 스크로블이 안 올라감 | 설정 → 계정이 연결·활성화됐는지 확인하세요. 오프라인 감상은 자동으로 배달됩니다(`scrobble-queue.jsonl`에서 대기). 데몬은 시작할 때 계정을 읽으니, 연결한 뒤에는 데몬을 재시작하세요. |
| 키를 잘못 바꿔서 엉망이 됨 | 설정 → 일반 → **단축키 초기화**. |

그래도 막히면? [이슈를 열고](https://github.com/Ochichan/ytm-tui/issues) OS를 알려주세요.

---

## 로그인 & 파일 위치

**쿠키 (선택).** 거의 필요 없습니다 — 공개 곡은 익명으로 잘 검색되고 재생돼요. 멤버 전용/지역 제한 트랙(그리고 플레이리스트 전송/계정 플레이리스트)에 접근하려면 YouTube Music 쿠키를 **Netscape 형식**의 `cookies.txt`로 내보내(macOS: `~/Music/ytm-tui/cookies.txt`, Windows: `%USERPROFILE%\Music\ytm-tui\cookies.txt`) `ytt`를 재시작하세요. **그 파일은 비밀번호처럼 다루세요.** 설정 → 일반에서 경로를 지정할 수도 있습니다.

내보낼 땐 *시크릿 창 방식*이 아니면 몇 분 안에 죽습니다: **시크릿/프라이빗 창**을 열고, 거기서 music.youtube.com에 로그인하고, 그 탭에서 `cookies.txt`를 내보낸 뒤(내보내기 확장 프로그램의 시크릿 모드 허용 먼저), **시크릿 창을 닫으세요**. 브라우저가 사라진 세션은 로테이션되거나 로그아웃되지 않아요 — 평소 쓰는 브라우저에서 내보낸 쿠키는 세션이 로테이션되는 순간 죽고, 도구를 세게 쓰면 그 브라우저 로그인까지 풀릴 수 있습니다. 제대로 된 내보내기에는 `SAPISID`/`SID` 줄이 있어요. 방문자 전용(비로그인) 내보내기는 동작하지 않고, `ytt`가 그렇게 알려줍니다.

**설정 & 데이터.**
- 설정: `~/Library/Application Support/ytm-tui/config.json` (macOS) · `~/.config/ytm-tui/config.json` (Linux) · `%APPDATA%\ytm-tui\config.json` (Windows).
- 그 옆에: `playlists.json`(내 플레이리스트), `scrobble-queue.jsonl`(미배달 감상 기록), `transfers/`(이어하기 가능한 작업 체크포인트 + 리포트).
- 다운로드 기본 위치는 `~/Music/ytm-tui`. **Download dir** 설정이나 `YTM_DOWNLOAD_DIR`로 변경하세요.
- `GEMINI_API_KEY`와 `YTM_DOWNLOAD_DIR` 환경 변수는 실행 시 저장된 설정보다 우선합니다.

---

## 스페셜 땡스

🙏 **[@ZZNN75](https://github.com/ZZNN75)** 님께 진짜 QA 시간에 대한 큰 감사를 — 구석구석 찔러보고 일부러 부숴봐 주신 덕분에, 여러분은 그럴 필요가 없습니다. 여러분이 *만나지 않을* 거친 모서리 상당수는 이분이 먼저 부딪혀서 매끈해진 것들이에요. 🫡

## 라이선스

MIT. 포크하고, 배포하고, 뭐든 하세요.
