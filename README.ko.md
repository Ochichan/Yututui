# ytm-tui

[English](README.md) · **한국어** · [日本語](README.ja.md)

[![Release](https://img.shields.io/github/v/release/Ochichan/ytm-tui)](https://github.com/Ochichan/ytm-tui/releases)
[![License: MIT](https://img.shields.io/badge/license-MIT-8aadf4.svg)](LICENSE)

터미널 안에서 즐기는 YouTube Music — 빠르고, 키보드로 다루고, 램을 야금야금 먹는 브라우저 탭도 광고도 없습니다. 전부 세 글자 명령 하나로: `ytt`. Rust + ratatui. MIT.

### [▶ 라이브 데모 · 기능 전체 둘러보기 → ochichan.github.io/ytm-tui](https://ochichan.github.io/ytm-tui/)

> 🖼️ *데모 움짤 준비 중!*
<!-- 📸 채우는 법: docs/media/hero.gif 를 추가하고, 위의 "준비 중" 줄을 지운 뒤 아래 줄 주석을 해제하세요:
![검색, 재생, 진짜 앨범 아트와 싱크 가사가 터미널 하나에](docs/media/hero.gif)
-->

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

> `curl | bash`와 소스 빌드 방식은 `ytt`만 설치합니다 — 설치 후 `ytt doctor`로 뭐가 빠졌는지 확인하세요.
> **yt-dlp는 스스로 최신을 유지합니다.** YouTube는 매주 바뀌기 때문에 `ytt`는 자체 yt-dlp를 직접 관리하며(github.com에서 SHA-256 검증), {관리형, 시스템} 중 더 최신 쪽을 사용합니다. `ytt tools status` / `ytt tools update`, 끄려면 `config.json`에 `"tools": {"ytdlp_managed": false}`.
> Scoop(Windows)과 brew(macOS)에는 **YtmTui Tray** 미니 플레이어도 함께 들어갑니다. 로그인 시 자동 실행은 선택 사항: `ytt-tray --install-startup`.

## 빠른 시작

```sh
ytt
```

1. **`s`** 를 누르고, 곡 제목을 입력한 뒤 **`Enter`**.
2. **`↑`/`↓`** 로 이동하고 **`Enter`** 로 재생.
3. 언제든 **`?`** 를 누르면 항상 최신인 전체 키 목록이 나옵니다.

끝. 음악 나옵니다. (뭔가 이상하면 **`ytt doctor`** 가 정확히 뭘 고칠지 알려줘요.)

## 둘러보기

스크린샷과 움짤이 곧 이 자리에 들어옵니다 — 그동안은 **[기능 둘러보기 페이지](https://ochichan.github.io/ytm-tui/)** 에서 전부 라이브로, 자세히 볼 수 있어요.

<!-- 📸 미디어 넣으실 분께: docs/media/ 폴더에 아래 이름 그대로 파일을 넣어주세요:
hero.gif · player.png · djgem.gif · assistant.gif · video.gif · sources.png · radio.png ·
everywhere.png · themes.gif · animations.gif · retro.png · transfer.gif
같은 파일이 README.md / README.ko.md / README.ja.md 세 곳에 함께 쓰입니다. 아래 각 슬롯마다
한 줄 안내가 있고, 더 넣고 싶으면 슬롯 블록을 복사해서 추가하면 됩니다. -->

### 플레이어 — 진짜 앨범 아트 & 싱크 가사

실제 커버 이미지가 터미널에 그대로 그려집니다(Kitty/Sixel/iTerm2 자동 감지). **`Shift+L`** 로 그 아래에 시간 싱크 가사가 흐릅니다.

> 🖼️ *스크린샷 준비 중!*
<!-- 📸 채우는 법: docs/media/player.png 를 추가하고, 위의 "준비 중" 줄을 지운 뒤 아래 줄 주석을 해제하세요:
![앨범 아트와 싱크 가사가 있는 플레이어](docs/media/player.png)
-->

### DJ Gem 스트리밍

**`Ctrl+R`** 을 누르면 지금 듣는 곡을 중심으로 끝없는 스테이션을 만들어줍니다 — **`w`** 를 누르면 각 곡을 고른 이유를 쉬운 말로 보여줘요.

> 🖼️ *움짤 준비 중!*
<!-- 📸 채우는 법: docs/media/djgem.gif 를 추가하고, 위의 "준비 중" 줄을 지운 뒤 아래 줄 주석을 해제하세요:
!["이 곡을 고른 이유" 패널과 함께하는 DJ Gem 스트리밍](docs/media/djgem.gif)
-->

### DJ Gem 어시스턴트 *(선택)*

**`g`** 를 누르고 말로 시키세요: *"lo-fi 틀어줘", "비 오는 날 플레이리스트 만들어줘"*. 무료 Gemini 키가 필요하고, 나머지 기능은 키 없이도 전부 동작합니다.

> 🖼️ *움짤 준비 중!*
<!-- 📸 채우는 법: docs/media/assistant.gif 를 추가하고, 위의 "준비 중" 줄을 지운 뒤 아래 줄 주석을 해제하세요:
![DJ Gem 어시스턴트에게 말로 음악을 부탁하는 모습](docs/media/assistant.gif)
-->

### 터미널 위에 떠 있는 뮤직비디오

**`v`** 를 누르면 작은 mpv 창에 뮤직비디오가 뜹니다. *영상 자동 이어재생*을 켜면 다음 곡의 영상으로 알아서 이어집니다.

> 🖼️ *움짤 준비 중!*
<!-- 📸 채우는 법: docs/media/video.gif 를 추가하고, 위의 "준비 중" 줄을 지운 뒤 아래 줄 주석을 해제하세요:
![터미널 위에 떠 있는 뮤직비디오](docs/media/video.gif)
-->

### 카탈로그 여섯, 검색창 하나 — 그리고 라디오 모드

검색에서 **`Tab`** 을 누르면 YouTube Music, SoundCloud, Audius, Jamendo, Internet Archive, Radio Browser를 오갑니다(전부 한꺼번에도 가능). **`Alt+Shift+R`** 은 앱 전체를 인터넷 라디오 튜너로 바꿔요.

> 🖼️ *스크린샷 준비 중!*
<!-- 📸 채우는 법: docs/media/sources.png 를 추가하고, 위의 "준비 중" 줄을 지운 뒤 아래 두 주석을 해제하세요:
![검색창 하나로 여섯 카탈로그를 검색](docs/media/sources.png)
-->
<!-- 📸 채우는 법: docs/media/radio.png 를 추가하고 주석 해제:
![인터넷 라디오 튜너가 된 라디오 모드](docs/media/radio.png)
-->

### 어디서든 제어

미디어 키, macOS Control Center, Windows SMTC + 트레이 미니 플레이어, Linux MPRIS, 아무 셸에서나 `ytt -r` — 아예 터미널 없는 headless 데몬까지.

> 🖼️ *스크린샷 준비 중!*
<!-- 📸 채우는 법: docs/media/everywhere.png 를 추가하고, 위의 "준비 중" 줄을 지운 뒤 아래 줄 주석을 해제하세요:
![OS 통합: 트레이 미니 플레이어, Control Center, SMTC, MPRIS](docs/media/everywhere.png)
-->

### 내 마음대로

테마 13종(색 역할 34개 전부 hex로 편집 가능), 그리고 애니메이션 25종 — 빙글빙글 도는 ASCII 도넛까지 포함해서요.

> 🖼️ *움짤 준비 중!*
<!-- 📸 채우는 법: docs/media/themes.gif 를 추가하고, 위의 "준비 중" 줄을 지운 뒤 아래 두 주석을 해제하세요:
![내장 테마를 하나씩 넘겨보기](docs/media/themes.gif)
-->
<!-- 📸 채우는 법: docs/media/animations.gif 를 추가하고 주석 해제:
![도는 ASCII 도넛을 포함한 애니메이션들](docs/media/animations.gif)
-->

### 레트로 모드

토글 하나로 모든 것이 CP437 안전이 됩니다 — 맨몸 리눅스 콘솔이나 낡은 SSH 세션용, 앨범 아트도 정직한 ASCII 아트로.

> 🖼️ *스크린샷 준비 중!*
<!-- 📸 채우는 법: docs/media/retro.png 를 추가하고, 위의 "준비 중" 줄을 지운 뒤 아래 줄 주석을 해제하세요:
![ASCII 앨범 아트가 있는 레트로 모드](docs/media/retro.png)
-->

### Spotify가 명령 한 줄로 이사 옵니다

`ytt transfer import <url>` — 체크포인트, 이어하기, 애매한 곡은 매치 리포트로. 설정 방법은 아래 참고 자료에.

> 🖼️ *움짤 준비 중!*
<!-- 📸 채우는 법: docs/media/transfer.gif 를 추가하고, 위의 "준비 중" 줄을 지운 뒤 아래 줄 주석을 해제하세요:
![Spotify 플레이리스트가 명령 한 줄로 이사 오는 모습](docs/media/transfer.gif)
-->

### 그리고 나머지

- **다운로드** — `d` 를 누르면 커버 아트와 태그가 박힌 m4a로 저장되어 오프라인 재생됩니다.
- **스크로블링** — Last.fm / ListenBrainz, 크래시에도 안전한 오프라인 큐와 함께.
- **10밴드 EQ** 프리셋 + 라우드니스 노멀라이즈.
- 모든 키 재설정 가능, UI 전체 마우스 지원, 인터페이스는 English & 한국어.

**모든 것의 자세한 이야기 → [기능 둘러보기](https://ochichan.github.io/ytm-tui/).**

## 필수 키

앱에서 **`?`** 를 누르면 완전한 라이브 치트시트가 나옵니다 — *내가 바꾼* 키 그대로 반영되고, 모든 키는 재설정할 수 있어요(설정 → 핫키). 핵심만:

| 키 | 동작 |
| --- | --- |
| `Space` | 재생 / 일시정지 |
| `,` / `.` | 이전 / 다음 |
| `←` / `→` · `↑` / `↓` | 탐색 · 볼륨 |
| `s` | 검색 (`Tab` 으로 카탈로그 선택) |
| `l` / `c` | 라이브러리 / 큐 |
| `f` / `d` | 즐겨찾기 / 다운로드 |
| `Shift+L` | 싱크 가사 |
| `v` | 뮤직비디오 오버레이 |
| `Ctrl+R` | DJ Gem 스트리밍 |
| `g` | DJ Gem 어시스턴트 |
| `o` | 설정 |
| `Ctrl+Q` | 종료 |

> **한글 자판이세요?** 단축키가 두벌식 자모를 알아듣습니다(`ㅂ` 은 `q` 처럼) — 입력기를 바꿀 필요가 없어요. 마우스가 편하면 화면의 모든 것이 클릭되고, 휠은 볼륨을 탑니다.

## 참고 자료

<details>
<summary><b>원격 제어 & 데몬</b></summary>

`ytt` 가 재생 중이면 아무 셸에서나 제어할 수 있습니다:

```sh
ytt -r pp                  # 재생 / 일시정지   (별칭: toggle, play, pause)
ytt -r next / prev         # 곡 이동
ytt -r volume 40           # 볼륨 지정; up / down 도 가능
ytt -r seek-to 90          # 1:30 지점으로 점프
ytt -r streaming on        # 무한 스트리밍: on / off / toggle
ytt -r play "lofi"         # 데몬: 검색해서 첫 결과 재생
ytt -r status              # 한 줄 "지금 재생 중" (--json 스크립트용)
```

i3 / sway 미디어 키 연결: `bindsym XF86AudioPlay exec ytt -r pp`.

터미널 없는 재생은 headless 데몬으로:

```sh
ytt daemon start --resume   # 저장된 큐/세션을 복원해 재생
ytt daemon stop             # 데몬 중지 + mpv 정리
```

데몬에서도 스트리밍, 스크로블링, OS 미디어 컨트롤이 그대로 동작합니다. `ytt` 를 두 번 실행해도 두 번째 플레이어가 생기지 않아요(정말 필요하면 `ytt --new-instance`). 전체 명령: `ytt -r --help`, `ytt daemon --help`.

</details>

<details>
<summary><b>스크로블링 설정 (Last.fm / ListenBrainz)</b></summary>

`ytt` 는 실제로 들은 것만 스크로블합니다 — 표준 하프트랙/4분 규칙, 좋아요→love 동기화, 그리고 네트워크를 시도하기 *전에* 디스크에 먼저 적히는 오프라인 큐(크래시에도 잃지 않아요). TUI와 데몬 모두에서 동작합니다.

- **Last.fm** — 설정 → **계정** → 브라우저에서 승인, 또는 `ytt auth lastfm`. 자가 빌드 바이너리는 `config.json`의 `scrobble.lastfm.api_key` / `api_secret`으로 직접 설정 가능([API 계정 만들기](https://www.last.fm/api/account/create)).
- **ListenBrainz** — [사용자 토큰](https://listenbrainz.org/settings/)을 설정 → 계정에 붙여넣거나 `ytt auth listenbrainz <token>`. 셀프 호스팅은 `scrobble.listenbrainz.api_url` 설정.
- 아직 배달 안 된 감상 기록은 설정 파일 옆 `scrobble-queue.jsonl`에서 대기하다가 자동으로 배달됩니다.

</details>

<details>
<summary><b>Spotify 가져오기 / 내보내기</b></summary>

```sh
ytt auth spotify --client-id <YOUR-CLIENT-ID>   # 최초 1회 PKCE 브라우저 연결
ytt transfer import <spotify-url-or-id>          # → 새 YTM 플레이리스트
ytt transfer import liked --to likes             # Spotify 좋아요 → YTM 좋아요 (순서 유지)
ytt transfer export ytm:<id> --to spotify        # 반대 방향
ytt transfer backup --dir ~/music-backup --csv   # 모든 YTM 플레이리스트 → JSON (+CSV)
ytt transfer resume <job-id>                     # 레이트 리밋/중단 후 이어하기
```

TUI 안에서도 됩니다: 설정 → **계정** → *Import from Spotify…* — 음악은 계속 들으면서요.

**최초 1회 설정.** Development Mode의 Spotify 앱은 자기 허용 목록에 있는 계정만 받아주므로, 본인의 (무료) 앱을 하나 만듭니다: [developer.spotify.com/dashboard](https://developer.spotify.com/dashboard)에서 앱을 만들고, 리디렉트 URI로 `http://127.0.0.1:9271/callback`을 **정확히** 추가하고(루프백 IP, `localhost` 아님; 포트는 `spotify.redirect_port`), *User Management*에 본인 계정을 추가한 뒤, Client ID를 설정 → 계정에 붙여넣으세요. 클라이언트 시크릿은 없습니다 — PKCE는 시크릿을 안 써요.

매칭은 메타데이터 기반입니다(NFKC 정규화, CJK 안전). 애매한 곡은 조용히 때려 맞추는 대신 작업 리포트에 남습니다 — `--take-best` / `--min-score`로 다시 돌리거나, 큰 플레이리스트는 `--dry-run`으로 확인한 뒤 `ytt transfer resume <job-id>`로 쓰세요.

</details>

<details>
<summary><b>로그인 쿠키 & 파일 위치</b></summary>

**쿠키 (선택).** 공개 곡은 익명으로 잘 재생됩니다 — 멤버 전용/지역 제한 트랙과 계정 플레이리스트에만 필요해요. YouTube Music 쿠키를 **Netscape 형식**으로 `~/Music/ytm-tui/cookies.txt`(Windows: `%USERPROFILE%\Music\ytm-tui\cookies.txt`)에 내보내고 재시작하세요. **그 파일은 비밀번호처럼 다루고**, *시크릿 창 방식*으로 내보내세요: 시크릿 창에서 로그인하고, 그 탭에서 `cookies.txt`를 내보낸 뒤, 창을 닫습니다 — 브라우저가 사라진 세션은 로테이션되거나 로그아웃되지 않아요. 제대로 된 내보내기에는 `SAPISID`/`SID` 줄이 있습니다.

**설정 & 데이터.**

- 설정: `~/Library/Application Support/ytm-tui/config.json` (macOS) · `~/.config/ytm-tui/config.json` (Linux) · `%APPDATA%\ytm-tui\config.json` (Windows) — 그 옆에 `playlists.json`, `scrobble-queue.jsonl`, `transfers/`.
- 다운로드: `~/Music/ytm-tui` — **Download dir** 설정이나 `YTM_DOWNLOAD_DIR`로 변경.
- `GEMINI_API_KEY`와 `YTM_DOWNLOAD_DIR` 환경 변수는 실행 시 저장된 설정보다 우선합니다.

</details>

<details>
<summary><b>문제 해결</b></summary>

| 증상 | 해결 |
| --- | --- |
| 아무것도 재생되지 않거나 재생 시 오류 | mpv 또는 yt-dlp가 없습니다 — `ytt doctor` 실행. |
| `ytt: command not found` | 새 터미널을 여세요. 그래도 안 되면 설치기가 출력한 `PATH` 줄을 추가. |
| 어제는 됐는데 오늘은 안 됨 | YouTube가 뭔가 바꿨어요 — `ytt tools update` 후 `ytt tools status`. |
| 특정 곡만 재생 안 됨 | 로그인이 필요할 수 있어요 — 위의 쿠키 항목 참고. |
| 앨범 아트가 안 보임 | 기본은 꺼짐: 설정 → 일반 → **앨범 아트** 켜고 재시작. |
| Control Center / SMTC / MPRIS에 안 나옴 | 설정 → 재생 → **OS 미디어 컨트롤** 확인; 뭔가 한 번 재생된 뒤부터 표시됩니다. |
| 플라이아웃에 "알 수 없는 앱" / 항목 2개 | `ytt register-media-identity`를 한 번 실행 (항목 2개 = mpv 자체 미디어 세션; mpv ≥ 0.39에서는 자동으로 꺼줍니다). |
| DJ Gem이 응답 안 함 | 설정 → DJ Gem에 무료 Gemini 키를 넣고 **Enable DJ Gem**을 켜세요. |
| Spotify 403 | Spotify 개발자 대시보드의 *User Management*에 본인 계정을 추가하세요. |
| 스크로블이 안 올라감 | 설정 → 계정 확인; 데몬은 시작할 때 계정을 읽으니 연결 후 재시작하세요. |
| 키를 잘못 바꿔서 엉망이 됨 | 설정 → 일반 → **단축키 초기화**. |

그래도 막히면? [이슈를 열고](https://github.com/Ochichan/ytm-tui/issues) OS를 알려주세요.

</details>

## 감사 & 라이선스

🙏 **[@ZZNN75](https://github.com/ZZNN75)** 님께 진짜 QA 시간에 대한 큰 감사를 — 여러분이 *만나지 않을* 거친 모서리들은 이분이 먼저 부딪혀서 매끈해진 것들이에요. 🫡

MIT. 포크하고, 배포하고, 뭐든 하세요.
