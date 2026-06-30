# ytm-tui

[English](README.md) · **한국어** · [日本語](README.ja.md)

### [▶ 라이브 데모 · 기능 둘러보기 → ochichan.github.io/ytm-tui](https://ochichan.github.io/ytm-tui/)

터미널 안에서 즐기는 YouTube Music. 빠르고, 키보드로 다루고, 램을 야금야금 먹는 브라우저 탭도 광고도 없습니다. AI 라디오, 진짜 앨범 아트, 원격 제어까지 — 전부 세 글자 명령 하나로: `ytt`.

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
| **Linux** — 그 외 | `curl -fsSL https://github.com/Ochichan/ytm-tui/releases/latest/download/install.sh \| sh` |
| **소스에서 빌드** | `./install.sh --build` ([Rust](https://rustup.rs) 필요) |

> `curl | sh`와 소스 빌드 방식은 `ytt`만 설치합니다. 보조 프로그램은 직접 설치하거나(`brew install mpv yt-dlp ffmpeg`, `sudo apt install mpv yt-dlp ffmpeg`, `sudo pacman -S mpv yt-dlp ffmpeg`) — 설치 후 `ytt doctor`로 뭐가 빠졌는지 확인하세요.

---

## 빠른 시작

```sh
ytt
```

1. **`/`** 누르고, 곡 이름 입력 후 **`Enter`**.
2. **`↑`/`↓`** 로 이동, **`Enter`** 로 재생.
3. 언제든 **`?`** 를 누르면 전체 단축키 목록(항상 최신).

끝. 음악이 나옵니다.

> **뭔가 이상한가요?** **`ytt doctor`** 를 실행하면 mpv, yt-dlp, ffmpeg를 점검하고 정확히 뭘 고쳐야 할지 알려줍니다. `ytt: command not found` 가 뜨면 터미널 창을 새로 열어 `PATH`가 반영되게 하세요.

---

## 무엇을 할 수 있나요

- **AI 라디오** — **`Ctrl+R`** 한 번이면 지금 듣는 곡을 중심으로 끝없이 이어지는 라디오가 시작됩니다. 분위기는 셋 중에서: Focused, Balanced, Discovery. **`w`** 를 누르면 각 곡을 고른 이유를 쉬운 말로 보여줍니다.
- **진짜 앨범 아트** — 지원하는 터미널이라면 플레이어에 실제 커버 이미지를 그려줍니다. 그 아래로는 시간 동기화된 가사가 흐르죠(**`Shift+L`**).
- **원격 제어** — 다른 터미널이나 미디어 키로 조종: `ytt -r pp`, `ytt -r next`, `ytt -r status`.
- **검색 · 보관함 · 큐** — **`/`** 검색, **`l`** 보관함(즐겨찾기·기록·다운로드), **`c`** 큐.
- **내 마음대로** — 테마 11종, 모든 색을 hex로 조정, 모든 키 재설정, 10밴드 EQ, 그리고 고요한 정지 화면부터 빙글빙글 도는 ASCII 도넛까지의 애니메이션.
- **AI 어시스턴트** *(선택)* — **`a`** 를 누르고 말로 시키세요: *"로파이 좀 틀어줘", "신나는 곡 세 개 큐에 넣어줘"*. 무료 Google Gemini 키가 필요하며, 나머지 기능은 키 없이도 모두 동작합니다.
- **다운로드** — **`d`** 로 곡을 저장해 오프라인에서 재생.

앱 인터페이스는 **영어와 한국어**를 지원합니다(설정 → 일반 → 언어). 이 README는 [English](README.md), [日本語](README.ja.md) 로도 제공됩니다.

---

## 핵심 단축키

앱에서 **`?`** 를 누르면 전체 치트시트가 뜨고 — *내가 바꾼 키* 그대로 반영됩니다. 아래 모든 키는 재설정 가능합니다(설정 → 단축키). 기본기는 이렇습니다:

| 키 | 기능 |
| --- | --- |
| `Space` | 재생 / 일시정지 |
| `←` / `→` | 뒤로 / 앞으로 탐색 |
| `↑` / `↓` | 볼륨 올리기 / 내리기 |
| `n` / `p` | 다음 / 이전 곡 |
| `/` | 검색 |
| `l` | 보관함 |
| `c` | 큐 |
| `f` | 즐겨찾기 / 평가 |
| `d` | 다운로드 |
| `Ctrl+R` | AI 라디오 켜기/끄기 |
| `Shift+L` | 가사 |
| `a` | AI 어시스턴트 |
| `w` | AI가 이 곡들을 고른 이유 |
| `,` | 설정 |
| `?` | 전체 단축키 목록 |
| `Ctrl+Q` | 종료 |

> **한글 키보드?** 단축키가 두벌식 자모를 알아들어서 `ㅂ` 은 `q`, `ㄱ` 은 `r` 처럼 동작합니다 — 입력기를 바꿀 필요 없어요.

---

## 원격 제어

`ytt` 가 재생 중이면 다른 터미널 — 또는 미디어 키 — 에서 `ytt -r` 로 조종할 수 있습니다:

```sh
ytt -r pp          # 재생 / 일시정지
ytt -r next        # 다음 곡
ytt -r radio on    # 라디오 켜기
ytt -r status      # 한 줄 "지금 재생 중"
ytt -r quit        # 멈추고 종료
```

미디어 키에 연결(i3 / sway):

```
bindsym XF86AudioPlay exec ytt -r pp
bindsym XF86AudioNext exec ytt -r next
```

`ytt` 를 두 번 실행해도 스피커를 두고 다투는 두 번째 플레이어가 생기지 않고, 이미 켜진 쪽을 어떻게 부를지만 알려줍니다. (정말 두 개를 원하면 `ytt --new-instance`.) 전체 명령은 `ytt -r --help`.

---

## 문제 해결

| 증상 | 해결 |
| --- | --- |
| 재생이 안 되거나 재생 즉시 오류 | mpv 또는 yt-dlp가 없거나 `PATH`에 없습니다. `ytt doctor` 실행. |
| `ytt: command not found` | 터미널을 새로 여세요. 그래도면 설치 폴더가 `PATH`에 없는 것 — 설치 시 추가할 줄을 출력해 줍니다. |
| 어제는 되던 게 오늘 안 됨 | YouTube가 뭔가 바꿨습니다 — yt-dlp 업데이트(`brew upgrade yt-dlp`, `scoop update yt-dlp`, 또는 패키지 매니저). |
| 특정 곡만 재생 안 됨 | 로그인이 필요할 수 있습니다 — 아래 쿠키 참고. |
| 앨범 아트가 안 보임 | 기본 꺼짐이며 터미널마다 다릅니다. **앨범 아트**(설정 → 일반)를 켜고 재시작. |
| AI가 응답 안 함 | 설정 → AI 에 무료 Gemini 키를 넣고 **AI 사용**을 켜세요. |
| 키를 바꿨다가 엉망이 됨 | 설정 → 일반 → **단축키 초기화**. |

그래도 막히면 [이슈를 남겨주세요](https://github.com/Ochichan/ytm-tui/issues). OS를 함께 적어주시면 좋습니다.

---

## 로그인 & 파일 위치

**쿠키(선택).** 대부분 필요 없습니다 — 공개된 곡은 익명으로도 검색·재생이 잘 됩니다. 멤버 전용이나 지역 제한 곡에 접근하려면 YouTube Music 쿠키를 **Netscape 형식**으로 `cookies.txt` 에 내보내고(macOS: `~/Music/ytm-tui/cookies.txt`, Windows: `%USERPROFILE%\Music\ytm-tui\cookies.txt`) `ytt` 를 재시작하세요. **이 파일은 비밀번호처럼 다루세요.** 설정 → 일반 에서 다른 경로를 지정할 수도 있습니다.

**설정 & 다운로드.**
- 설정 파일: `~/Library/Application Support/ytm-tui/config.json` (macOS) · `~/.config/ytm-tui/config.json` (Linux) · `%APPDATA%\ytm-tui\config.json` (Windows).
- 다운로드 기본 위치는 `~/Music/ytm-tui` 이며, **다운로드 폴더** 설정이나 `YTM_DOWNLOAD_DIR` 로 바꿀 수 있습니다.
- `GEMINI_API_KEY` 와 `YTM_DOWNLOAD_DIR` 환경 변수는 실행 시 저장된 설정보다 우선합니다.

---

## 특별히 감사한 분

🙏 **[@ZZNN75](https://github.com/ZZNN75)** 님께 큰 감사를 — 구석구석 찔러보고 일부러 부숴가며 진짜 QA 시간을 들여주셨습니다. 여러분이 *겪지 않을* 거친 부분들이 매끄러운 건, 그분이 먼저 겪고 알려준 덕분입니다. 🫡

## 라이선스

MIT. 포크하든, 배포하든, 마음대로 하세요.
