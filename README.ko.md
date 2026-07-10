# YuTuTui!

[English](README.md) · **한국어** · [日本語](README.ja.md)

[![Release](https://img.shields.io/github/v/release/Ochichan/Yututui)](https://github.com/Ochichan/Yututui/releases)
[![CI](https://img.shields.io/github/actions/workflow/status/Ochichan/Yututui/ci-pr.yml?branch=main&label=CI)](https://github.com/Ochichan/Yututui/actions/workflows/ci-pr.yml)
[![Downloads](https://img.shields.io/github/downloads/Ochichan/Yututui/total?color=f6c177)](https://github.com/Ochichan/Yututui/releases)
[![License: MIT](https://img.shields.io/badge/license-MIT-8aadf4.svg)](LICENSE)

터미널 안에서 즐기는 YouTube Music — 빠르고, 키보드로 다루고, 램을 야금야금 먹는 브라우저 탭도 광고도 없습니다. 전부 세 글자 명령 하나로: `ytt`. Rust + ratatui. MIT.

Public beta: 매일 쓰기엔 충분히 안정적이지만, 아직 빠르게 움직이는 중입니다.

### [▶ 라이브 데모 · 기능 전체 둘러보기 → ochichan.github.io/Yututui](https://ochichan.github.io/Yututui/)

**📖 터미널이 낯설다면?** [친절한 사용 설명서](MANUAL.ko.md)가 모든 모드를 — 음악, 라디오, 로컬 덱, Spotify 이사까지 — 전문 용어 없이 한 걸음씩 안내합니다.

> 🖼️ *데모 움짤 준비 중!*
<!-- 📸 채우는 법: docs/media/hero.gif 를 추가하고, 위의 "준비 중" 줄을 지운 뒤 아래 줄 주석을 해제하세요:
![검색, 재생, 진짜 앨범 아트와 싱크 가사가 터미널 하나에](docs/media/hero.gif)
-->

---

## 설치

각 명령은 `ytt`와 보조 프로그램(mpv, yt-dlp, ffmpeg)을 **한 번에** 함께 설치합니다.

| OS | 한 줄이면 끝 |
| --- | --- |
| **macOS** | `brew install Ochichan/tap/yututui` |
| **Windows** | `scoop bucket add extras; scoop bucket add yututui https://github.com/Ochichan/scoop-bucket; scoop install yututui` |
| **Linux** — 아무 배포판, [Nix](https://nixos.org/download) | `nix run github:Ochichan/Yututui` |
| **Linux** — Arch | `yay -S yututui-bin` |
| **Linux** — 그 외 | 아래 설치 스크립트 실행 |
| **소스에서 빌드** | `./install.sh --build` ([Rust](https://rustup.rs) 필요) |

> Arch AUR 게시는 잠시 보류 중입니다. `yututui-bin`이 올라올 때까지 Nix나 설치 스크립트를 쓰세요.

```sh
curl -fsSL https://raw.githubusercontent.com/Ochichan/Yututui/main/install.sh | bash
```

Windows 직접 설치:

```powershell
irm https://raw.githubusercontent.com/Ochichan/Yututui/main/install.ps1 | iex
```

그다음 `ytt`를 실행하세요. 뭔가 이상하면 `ytt doctor`가 정확히 뭘 고칠지 알려줍니다 — 자세한 건 [문제 해결](#문제-해결)에.

<details>
<summary><b>Tray 보조 앱 (macOS / Windows)</b></summary>

macOS와 Windows 릴리스에는 메뉴바/알림 영역 미니 플레이어인 `yututray`이 들어갑니다.

| 채널 | 설치되는 것 | Tray 시작 |
| --- | --- | --- |
| macOS Homebrew | `ytt`, `yututray`, 런타임 도구 | `yututray --background` |
| Windows Scoop | `ytt.exe`, `yututray.exe`, 런타임 도구, 시작 메뉴 바로가기 | `yututray --background` 또는 **YuTuTray!** |
| 직접 설치 / 소스 빌드 스크립트 | `ytt`; macOS/Windows는 `yututray`도 함께 설치 | `yututray --background` |
| Linux | MPRIS 미디어 연동이 들어간 `ytt` | 별도 tray 앱 없음 |

로그인 시 자동 실행은 선택 사항입니다: `yututray --install-startup`.

</details>

## 빠른 시작

```sh
ytt
```

1. **`s`** 를 누르고, 곡 제목을 입력한 뒤 **`Enter`**.
2. **`↑`/`↓`** 로 이동하고 **`Enter`** 로 재생.
3. 언제든 **`?`** 를 누르면 항상 최신인 전체 키 목록이 나옵니다.

끝. 음악 나옵니다.

## 둘러보기

아래 모든 기능은 **[기능 둘러보기 페이지](https://ochichan.github.io/Yututui/)** 에서 라이브로, 자세히 볼 수 있어요.

<!-- 📸 미디어 넣으실 분께: docs/media/ 폴더에 아래 이름 그대로 파일을 넣어주세요:
hero.gif · player.png · lyrics.gif · search.gif · sources.png · djgem.gif · assistant.gif ·
video.gif · radio.png · radio-id.gif · library.png · queue.png · downloads.png ·
localdeck.png · everywhere.png · tray.png · themes.gif · animations.gif · eq.png ·
retro.png · transfer.gif · help.png
같은 파일이 README.md / README.ko.md / README.ja.md 세 곳에 함께 쓰입니다. 아래 각 슬롯마다
한 줄 안내가 있고, 더 넣고 싶으면 슬롯 블록을 복사해서 추가하면 됩니다. -->

### 플레이어 — 진짜 앨범 아트 & 싱크 가사

실제 커버 이미지가 터미널에 그대로 그려집니다(Kitty/Sixel/iTerm2 자동 감지). **`Shift+L`** 로 그 아래에 시간 싱크 가사가 흐릅니다.

> 🖼️ *스크린샷 준비 중!*
<!-- 📸 채우는 법: docs/media/player.png 를 추가하고, 위의 "준비 중" 줄을 지운 뒤 아래 줄 주석을 해제하세요:
![앨범 아트와 싱크 가사가 있는 플레이어](docs/media/player.png)
-->
<!-- 📸 채우는 법: docs/media/lyrics.gif 를 추가하고 주석 해제:
![플레이어 아래로 흐르는 시간 싱크 가사](docs/media/lyrics.gif)
-->

### 카탈로그 여섯, 검색창 하나

검색에서 **`Tab`** 을 누르면 YouTube Music, SoundCloud, Audius, Jamendo, Internet Archive, Radio Browser를 오갑니다 — 전부 한꺼번에도 가능하고, 결과마다 `[SRC]` 태그가 붙어요.

> 🖼️ *스크린샷 준비 중!*
<!-- 📸 채우는 법: docs/media/search.gif 를 추가하고, 위의 "준비 중" 줄을 지운 뒤 아래 줄 주석을 해제하세요:
![검색어를 입력하고 결과를 재생하는 모습](docs/media/search.gif)
-->
<!-- 📸 채우는 법: docs/media/sources.png 를 추가하고 주석 해제:
![검색창 하나로 여섯 카탈로그를 검색](docs/media/sources.png)
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

**`v`** 를 누르면 작은 mpv 창에 뮤직비디오가 뜹니다. *영상 자동 이어재생*을 켜면 다음 곡의 영상으로 알아서 이어지고, mpv 창에서는 `Space`, `.`, `,`, `q`, `f`, `m`이 통합니다.

> 🖼️ *움짤 준비 중!*
<!-- 📸 채우는 법: docs/media/video.gif 를 추가하고, 위의 "준비 중" 줄을 지운 뒤 아래 줄 주석을 해제하세요:
![터미널 위에 떠 있는 뮤직비디오](docs/media/video.gif)
-->

### 라디오 모드 — 지금 나오는 곡까지 압니다

**`Alt+Shift+R`** 은 앱 전체를 인터넷 라디오 튜너로 바꿉니다. **`i`** 를 누르면 Gemini가 생방송에서 지금 나오는 곡의 이름을 알려주고, **`f`** 로 바로 즐겨찾기.

> 🖼️ *스크린샷 준비 중!*
<!-- 📸 채우는 법: docs/media/radio.png 를 추가하고, 위의 "준비 중" 줄을 지운 뒤 아래 줄 주석을 해제하세요:
![인터넷 라디오 튜너가 된 라디오 모드](docs/media/radio.png)
-->
<!-- 📸 채우는 법: docs/media/radio-id.gif 를 추가하고 주석 해제:
![i 를 눌러 라이브 라디오의 현재 곡을 식별하는 모습](docs/media/radio-id.gif)
-->

### 라이브러리, 큐 & 다운로드

라이브러리에서 바로 플레이리스트를 만들고(DJ Gem에게 시켜도 됩니다), **`c`** 로 큐를 띄우고, **`d`** 는 커버 아트·태그 박힌 m4a로 저장 — **`Shift+D`** 는 목록 통째로.

> 🖼️ *스크린샷 준비 중!*
<!-- 📸 채우는 법: docs/media/library.png 를 추가하고, 위의 "준비 중" 줄을 지운 뒤 아래 줄 주석을 해제하세요:
![플레이리스트·즐겨찾기·기록이 있는 라이브러리](docs/media/library.png)
-->
<!-- 📸 채우는 법: docs/media/queue.png 를 추가하고 주석 해제:
![플레이어 위에 뜬 큐 팝업](docs/media/queue.png)
-->
<!-- 📸 채우는 법: docs/media/downloads.png 를 추가하고 주석 해제:
![다운로드: 커버 아트와 태그가 박힌 m4a, 오프라인 재생](docs/media/downloads.png)
-->

### 로컬 덱 — 디스크 위 모든 음악의 오프라인 플레이어

라이브러리에서 **`Alt+Shift+L`** 을 누르면 다운로드와 로컬 파일을 위한 몰입형 플레이어가 열립니다 — 앨범, 아티스트, 장르, 스마트 리스트, 인터넷 불필요. 자세한 안내는 [사용 설명서](MANUAL.ko.md)에.

> 🖼️ *스크린샷 준비 중!*
<!-- 📸 채우는 법: docs/media/localdeck.png 를 추가하고, 위의 "준비 중" 줄을 지운 뒤 아래 줄 주석을 해제하세요:
![로컬 앨범을 둘러보는 로컬 덱](docs/media/localdeck.png)
-->

### 어디서든 제어

미디어 키, macOS Control Center, Windows SMTC + 트레이 미니 플레이어, Linux MPRIS, 아무 셸에서나 `ytt -r` — 아예 터미널 없는 headless 데몬까지.

> 🖼️ *스크린샷 준비 중!*
<!-- 📸 채우는 법: docs/media/everywhere.png 를 추가하고, 위의 "준비 중" 줄을 지운 뒤 아래 줄 주석을 해제하세요:
![OS 통합: 트레이 미니 플레이어, Control Center, SMTC, MPRIS](docs/media/everywhere.png)
-->
<!-- 📸 채우는 법: docs/media/tray.png 를 추가하고 주석 해제:
![메뉴바/트레이의 yututray 미니 플레이어](docs/media/tray.png)
-->

### 내 마음대로

테마 13종(색 역할 34개 전부 hex 편집), 애니메이션 25종 — 빙글빙글 도는 ASCII 도넛 포함 — 그리고 프리셋 있는 10밴드 EQ + 라우드니스 노멀라이즈.

> 🖼️ *움짤 준비 중!*
<!-- 📸 채우는 법: docs/media/themes.gif 를 추가하고, 위의 "준비 중" 줄을 지운 뒤 아래 줄 주석을 해제하세요:
![내장 테마를 하나씩 넘겨보기](docs/media/themes.gif)
-->
<!-- 📸 채우는 법: docs/media/animations.gif 를 추가하고 주석 해제:
![도는 ASCII 도넛을 포함한 애니메이션들](docs/media/animations.gif)
-->
<!-- 📸 채우는 법: docs/media/eq.png 를 추가하고 주석 해제:
![프리셋이 있는 10밴드 EQ](docs/media/eq.png)
-->

### 레트로 모드

토글 하나로 모든 것이 CP437 안전이 됩니다 — 맨몸 리눅스 콘솔이나 낡은 SSH 세션용, 앨범 아트도 정직한 ASCII 아트로.

> 🖼️ *스크린샷 준비 중!*
<!-- 📸 채우는 법: docs/media/retro.png 를 추가하고, 위의 "준비 중" 줄을 지운 뒤 아래 줄 주석을 해제하세요:
![ASCII 앨범 아트가 있는 레트로 모드](docs/media/retro.png)
-->

### Spotify가 명령 한 줄로 이사 옵니다

`ytt transfer import <url>` — 체크포인트, 이어하기, 애매한 곡은 매치 리포트로. 설정 방법은 아래 [참고 자료](#참고-자료)에 — 손잡고 처음부터 끝까지는 [사용 설명서](MANUAL.ko.md)가 안내합니다.

> 🖼️ *움짤 준비 중!*
<!-- 📸 채우는 법: docs/media/transfer.gif 를 추가하고, 위의 "준비 중" 줄을 지운 뒤 아래 줄 주석을 해제하세요:
![Spotify 플레이리스트가 명령 한 줄로 이사 오는 모습](docs/media/transfer.gif)
-->

### 단축키는 앱이 기억합니다

**`?`** 를 누르면 *내가 바꾼* 키 그대로 반영된 라이브 치트시트가 뜹니다 — 모든 키 재설정 가능, UI 전체 마우스 지원, 인터페이스는 English & 한국어.

> 🖼️ *스크린샷 준비 중!*
<!-- 📸 채우는 법: docs/media/help.png 를 추가하고, 위의 "준비 중" 줄을 지운 뒤 아래 줄 주석을 해제하세요:
![라이브 단축키 치트시트](docs/media/help.png)
-->

## 필수 키

앱에서 **`?`** 를 누르면 완전한 라이브 치트시트가 나옵니다 — *내가 바꾼* 키 그대로 반영되고, 모든 키는 재설정할 수 있어요(설정 → 핫키). 핵심만:

| 키 | 동작 |
| --- | --- |
| `Space` | 재생 / 일시정지 |
| `,` / `.` | 이전 / 다음 (mpv 영상 창에서도) |
| `←` / `→` · `↑` / `↓` | 탐색 · 볼륨 |
| `s` | 검색 (`Tab` 으로 카탈로그 선택) |
| `l` / `c` | 라이브러리 / 큐 |
| `↑`/`↓` 꾹 · `Shift`+`↑`/`↓` | 목록 빠르게 스크롤(가속) · 범위 선택 |
| `f` / `d` | 즐겨찾기 / 다운로드 (`Shift`+`↑`/`↓` 나 드래그로 여러 곡 선택 후 `d` → 한꺼번에) |
| `Shift+D` | 목록 / 플레이리스트 전체 다운로드 |
| `Shift+L` | 싱크 가사 |
| `v` | 뮤직비디오 오버레이 |
| `Ctrl+R` | DJ Gem 스트리밍 |
| `g` | DJ Gem 어시스턴트 |
| `o` | 설정 |
| `Ctrl+Q` | 종료 |

> **한글 자판이세요?** 단축키가 두벌식 자모를 알아듣습니다(`ㅂ` 은 `q` 처럼) — 입력기를 바꿀 필요가 없어요. 마우스가 편하면 화면의 모든 것이 클릭되고, 휠은 볼륨을 탑니다.

## 문제 해결

우선 언제나: **`ytt doctor`** 가 mpv, yt-dlp, ffmpeg를 점검하고 정확히 뭘 고칠지 알려줍니다. 더 깊게는 `ytt doctor --verbose`, 터미널 능력 확인은 `ytt doctor terminal --json`.

### 재생

| 증상 | 해결 |
| --- | --- |
| 아무것도 재생되지 않거나 재생 시 오류 | mpv 또는 yt-dlp가 없습니다 — `ytt doctor` 실행. |
| 어제는 됐는데 오늘은 안 됨 | YouTube가 뭔가 바꿨어요 — `ytt tools update` 후 `ytt tools status --why`; 관리형 업데이트가 문제면 `ytt tools use system`. |
| 여러 곡이 403/429 또는 "YouTube rejected the stream"으로 실패 | `ytt doctor --verbose`를 실행하고, [참고 자료](#참고-자료)의 쿠키 항목을 확인하고, 지원되는 JS 런타임이 있는지 보세요; 활성 yt-dlp는 `ytt tools status --why`로 확인. |
| 특정 곡만 재생 안 됨 | 로그인이 필요할 수 있어요 — [참고 자료](#참고-자료)의 쿠키 항목 참고. |
| 앱이 셸과 다른 yt-dlp를 실행함 | 의도된 동작입니다(관리형 복사본 vs `PATH`) — [참고 자료](#참고-자료)의 *yt-dlp 선택* 참고. |

### 설치 & 시작

| 증상 | 해결 |
| --- | --- |
| `ytt: command not found` | 새 터미널을 여세요. 그래도 안 되면 설치기가 출력한 `PATH` 줄을 추가. |
| 직접 설치 / 소스 빌드 후 보조 프로그램이 없음 | 한 줄 설치 스크립트는 `ytt`만 설치합니다 — `ytt doctor`가 뭘 어떻게 설치할지 알려줘요. |

### 화면 & 터미널

터미널 지원은 에뮬레이터마다 다릅니다 — YuTuTui!는 가능한 기능을 감지하고, 안 되면 fallback으로 내려갑니다. 환경 확인은 `ytt doctor terminal --json`, 자세한 표는 [terminal compatibility matrix](docs/terminal-compatibility.md).

| 증상 | 해결 |
| --- | --- |
| 앨범 아트가 안 보임 | 기본은 꺼짐: 설정 → 일반 → **앨범 아트** 켜고 재시작. |
| 터미널마다 앨범 아트/확대 동작이 다름 | `ytt doctor terminal --json`을 실행하고 [terminal matrix](docs/terminal-compatibility.md)와 비교하세요. |
| VS Code / Apple Terminal에서 앨범 아트가 각져 보임 | 그 터미널들엔 이미지 프로토콜이 없어요 — halfblock이 의도된 fallback입니다. |
| 맨몸 리눅스 콘솔·오래된 SSH에서 화면이 깨짐 | 레트로 모드를 켜세요(설정 → 그래픽): 모든 것이 CP437 안전으로 다시 그려지고, 앨범 아트는 ASCII 아트가 됩니다. |
| SSH / 맨몸 TTY에서 `v`(뮤직비디오)가 반응 없음 | 영상 오버레이는 mpv GUI 창입니다 — 데스크톱 세션이 필요해요. |

### Spotify 가져오기

| 증상 | 해결 |
| --- | --- |
| Spotify 403 / "허용 목록 없음" | Spotify 개발자 대시보드의 *User Management*에 본인 계정을 추가하고, Client ID 오타를 확인하세요. |
| 브라우저에 INVALID_CLIENT / 리디렉트 불일치 | 리디렉트 URI가 **정확히** 일치해야 합니다: `http://127.0.0.1:9271/callback` — `localhost` 아닌 IP, 올바른 포트, 끝에 슬래시 없음. |
| "could not listen on 127.0.0.1:9271" | 포트가 사용 중입니다. `config.json`의 `spotify.redirect_port`를 바꾸고 대시보드 리디렉트 URI도 맞추세요. |
| Connect를 눌렀는데 브라우저가 안 열림 | 헤드리스/SSH에서는 인증 URL이 클립보드에 복사되고 `spotify_auth_url.txt`에 저장됩니다 — 아무 브라우저에 붙여넣어 승인하세요. |
| Spotify 가져오기가 "YouTube Music 쿠키 필요"라고 함 | YTM 플레이리스트/좋아요로 가져오려면 로그인이 필요하지만, 로컬 라이브러리 플레이리스트로 가져오는 건 쿠키 없이 됩니다. [참고 자료](#참고-자료)의 쿠키 항목 참고. |

### 계정, 스크로블 & OS 연동

| 증상 | 해결 |
| --- | --- |
| 스크로블이 안 올라감 | 설정 → 계정 확인; 데몬은 시작할 때 계정을 읽으니 연결 후 재시작하세요. |
| Control Center / SMTC / MPRIS에 안 나옴 | 설정 → 재생 → **OS 미디어 컨트롤** 확인; 뭔가 한 번 재생된 뒤부터 표시됩니다. |
| 플라이아웃에 "알 수 없는 앱" / 항목 2개 | `ytt register-media-identity`를 한 번 실행 (항목 2개 = mpv 자체 미디어 세션; mpv ≥ 0.39에서는 자동으로 꺼줍니다). |
| 데스크톱 업데이트 알림이 안 보임 | 업데이트 안내는 About/상태줄에도 남습니다; 데스크톱 알림은 터미널, tmux, OS 알림 지원에 따라 best-effort로 동작합니다. |

### 그 외 전부

| 증상 | 해결 |
| --- | --- |
| DJ Gem이 응답 안 함 | 설정 → DJ Gem에 무료 Gemini 키를 넣고 **Enable DJ Gem**을 켜세요. |
| 키를 잘못 바꿔서 엉망이 됨 | 설정 → 일반 → **단축키 초기화**. |

그래도 막히면? [이슈를 열고](https://github.com/Ochichan/Yututui/issues) OS를 알려주세요.

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
ytt transfer import <url> --policy strict        # 더 보수적인 리뷰 중심 매칭
ytt transfer export ytm:<id> --to spotify        # 반대 방향
ytt transfer backup --dir ~/music-backup --csv   # 모든 YTM 플레이리스트 → JSON (+CSV)
ytt transfer resume <job-id>                     # 레이트 리밋/중단 후 이어하기
```

TUI 안에서도 됩니다: 설정 → **계정** → *Import from Spotify…* — 음악은 계속 들으면서요.

**최초 1회 설정 (~5분).** Development Mode의 Spotify 앱은 직접 허용 목록에 넣은 계정만 받아주므로, 각자 자기 무료 앱을 하나 만듭니다. 클라이언트 *시크릿*은 없습니다 — PKCE는 시크릿을 안 써요.

1. [developer.spotify.com/dashboard](https://developer.spotify.com/dashboard)에 로그인하고 **Create app**을 누릅니다.
2. **App name**과 **App description**은 아무거나 (예: `yututui`).
3. **Redirect URIs**에 `http://127.0.0.1:9271/callback`을 **정확히** 추가하고 **Add**를 누릅니다. 루프백 IP 리터럴 `127.0.0.1`이어야 하며 **`localhost`는 안 됩니다**(Spotify가 거부). 포트를 바꾸려면 `config.json`의 `spotify.redirect_port`를 설정하고 여기에도 맞추세요.
4. **Which API/SDKs are you planning to use?**에서 **Web API**를 체크합니다.
5. 약관에 동의하고 **Save**.
6. 앱 → **Settings**에서 **Client ID**를 복사합니다 (Client secret은 필요 없음).
7. **User Management**(앱 설정 안)를 열고 본인 계정을 추가합니다 — 이름 + Spotify 계정 이메일. Dev Mode 앱은 이렇게 허용된 유저를 최대 25명까지 받습니다.
8. ytt에서 **설정 → 계정 → Spotify**로 가서 Client ID를 붙여넣고 **Connect**를 누릅니다 (또는 `ytt auth spotify --client-id <ID>`). 브라우저에 Spotify 승인 페이지가 열리면 승인하면 끝. 브라우저가 안 열리는 헤드리스/SSH 환경에서는 URL이 클립보드에 복사되고 `spotify_auth_url.txt`에도 저장되니 아무 기기에서나 여세요.

매칭은 메타데이터 기반이며(NFKC 정규화, CJK 안전) Spotify 가져오기를 캐시 우선, 앨범 인지, YTM 카탈로그 우선으로 해석한 뒤에야 공개 YouTube 영상으로 fallback합니다. CLI 기본값은 `--policy balanced`; 보수적인 리뷰 중심 매칭은 `--policy strict`, 리뷰 행을 줄이려면 `--policy aggressive`, 일반 공개 업로드도 괜찮을 때만 `--allow-user-videos`. 애매한 곡은 조용히 때려 맞추는 대신 작업 리포트에 남습니다 — `--take-best` / `--min-score`로 다시 돌리거나, 큰 플레이리스트는 `--dry-run`으로 확인한 뒤 `ytt transfer resume <job-id>`로 쓰세요.

</details>

<details>
<summary><b>로그인 쿠키 & 파일 위치</b></summary>

**쿠키 (선택).** 공개 곡은 익명으로 잘 재생됩니다 — 멤버 전용/지역 제한 트랙과 계정 플레이리스트에만 필요해요. YouTube Music 쿠키를 **Netscape 형식**으로 `~/Music/yututui/cookies.txt`(Windows: `%USERPROFILE%\Music\yututui\cookies.txt`)에 내보내고 재시작하세요. **그 파일은 비밀번호처럼 다루고**, *시크릿 창 방식*으로 내보내세요: 시크릿 창에서 로그인하고, 그 탭에서 `cookies.txt`를 내보낸 뒤, 창을 닫습니다 — 브라우저가 사라진 세션은 로테이션되거나 로그아웃되지 않아요. 제대로 된 내보내기에는 `SAPISID`/`SID` 줄이 있습니다.

**설정 & 데이터.**

- 설정: `~/Library/Application Support/yututui/config.json` (macOS) · `~/.config/yututui/config.json` (Linux) · `%APPDATA%\yututui\config.json` (Windows) — 그 옆에 `playlists.json`, `scrobble-queue.jsonl`, `transfers/`.
- 다운로드: `~/Music/yututui` — **Download dir** 설정이나 `YTM_DOWNLOAD_DIR`로 변경.
- `GEMINI_API_KEY`와 `YTM_DOWNLOAD_DIR` 환경 변수는 실행 시 저장된 설정보다 우선합니다.

</details>

<details>
<summary><b>yt-dlp 선택</b></summary>

**yt-dlp는 스스로 최신을 유지합니다.** YouTube는 매주 바뀌기 때문에 `ytt`는 자체 yt-dlp를 직접 관리하며(github.com에서 SHA-256 검증), {관리형, 시스템} 중 더 최신 쪽을 사용합니다. 그래서 셸에서 `yt-dlp --version`으로 보이는 것과 다른 yt-dlp를 실행할 수 있어요. 실제 선택과 후보를 보려면:

```sh
ytt tools status --why
```

복구용 명령:

```sh
ytt tools update              # 관리형 복사본을 지금 갱신
ytt tools use system          # 관리형 yt-dlp를 무시하고 PATH 사용
ytt tools use managed         # 설치된 관리형 복사본에 고정
ytt tools use /path/to/yt-dlp # 특정 실행 파일에 고정
ytt tools unpin               # 기본 managed/system 선택 정책으로 복귀
```

`YTM_YTDLP`는 여전히 가장 강한 override입니다. OS 설정에서 값을 바꿨다면 새 터미널을 열거나 해당 환경 변수를 해제해야 `ytt tools use ...` 설정이 기대대로 적용됩니다.

앱 자체의 yt-dlp 호출은 기본적으로 여러분의 yt-dlp 설정 파일을 무시하므로, 셸 다운로드용 옵션이 파싱 출력을 깨지 않습니다. 앱 파싱 호출에도 yt-dlp 설정을 쓰려면 `YTM_YTDLP_USER_CONFIG=1`. mpv의 `ytdl_hook`을 통한 재생은 여전히 yt-dlp 설정을 따르고, 검색·플레이리스트 조회·메타데이터·프리페치 해석·다운로드만 기본적으로 무시합니다.

</details>

## 감사 & 라이선스

🙏 **[@ZZNN75](https://github.com/ZZNN75)** 님께 진짜 QA 시간에 대한 큰 감사를 — 여러분이 *만나지 않을* 거친 모서리들은 이분이 먼저 부딪혀서 매끈해진 것들이에요. 🫡

MIT. 포크하고, 배포하고, 뭐든 하세요.
