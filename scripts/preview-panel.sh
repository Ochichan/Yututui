#!/usr/bin/env bash
# Dev-only preview of the assembled mini player sources (src/desktop/panel.html +
# src/desktop/panel_assets/) outside the tray: replaces the host-spliced tokens and opens every
# theme in one wrapper page, each iframe sized exactly like the native window
# (PanelTheme::window_size) on a checkerboard that makes transparency visible.
#
#   scripts/preview-panel.sh [out-dir]
#
# The page's `send()` is a no-op without window.ipc, so clicking around (theme
# picker, ⋯ expansion, tama menu) works. To simulate live updates, open a
# panel-<theme>.html directly and paste into the devtools console e.g.:
#   window.ytmTuiApply({ ...window.__YTM_TUI_INITIAL__, paused: true })   // pet sleeps
#   window.ytmTuiApply({ ...window.__YTM_TUI_INITIAL__, connected: false })
#   window.ytmTuiApplyArt("data:image/png;base64,....")                  // art flash
# Not part of CI.
set -euo pipefail
cd "$(dirname "$0")/.."

OUT="${1:-$(mktemp -d /tmp/ytt-panel-preview.XXXXXX)}"
mkdir -p "$OUT"

python3 - "$OUT" <<'PY'
import copy
import json
import pathlib
import sys

out = pathlib.Path(sys.argv[1])
panel_root = pathlib.Path("src/desktop")
src = "".join(
    (panel_root / path).read_text()
    for path in (
        "panel_assets/document_start.html",
        "panel_assets/common.css",
        "panel_assets/cushion.css",
        "panel_assets/shared.css",
        "panel_assets/minimal.css",
        "panel_assets/tamagotchi.css",
        "panel_assets/accessibility.css",
        "panel_assets/body_start.html",
        "panel.html",
        "panel_assets/script_start.html",
        "panel_assets/ipc-state.js",
        "panel_assets/document_end.html",
    )
)

# A playing snapshot exercising every field the page reads.
payload = {
    "connected": True,
    "state": "playing",
    "title": "Longer Song Title For Marquee Testing",
    "artist": "Fox Artist",
    "stateLabel": "Playing",
    "ownerLabel": "Daemon",
    "queueLabel": "1 / 2",
    "volumeLabel": "80%",
    "volume": 80,
    "elapsedMs": 42000,
    "durationMs": 180000,
    "isLive": False,
    "queueRev": 41,
    "trackIdentity": "v8\\u001ffixture-track\\u001f7",
    "canSeek": True,
    "queue": [
        {"index": 0, "title": "Longer Song Title For Marquee Testing", "artist": "Fox Artist", "duration": "3:00", "current": True},
        {"index": 1, "title": "Second Song", "artist": "Fox Artist", "duration": "2:30", "current": False},
    ],
    "shuffle": True,
    "repeat": "all",
    "repeatLabel": "All",
    "paused": False,
    "streaming": True,
    "error": None,
    "canPlayback": True,
    "canVolume": True,
    "canManageQueue": True,
    "canToggleStreaming": True,
    "canStartDaemon": False,
    "canResumeDaemon": False,
    "canStopDaemon": True,
    "settings": {
        "autoplayStreaming": True,
        "streamingMode": "balanced",
        "streamingModeLabel": "Balanced",
        "streamingSource": "youtube",
        "streamingSourceLabel": "YouTube",
        "streamingSources": [{"value": "youtube", "label": "YouTube"}],
        "speedTenths": 10,
        "speedLabel": "1.0x",
        "seekSeconds": 10,
        "seekLabel": "10s",
        "normalize": False,
        "gapless": True,
        "aiEnabled": True,
        "radioMode": False,
        "canRadioMode": True,
    },
}

scenarios = {"playing": payload}

paused = copy.deepcopy(payload)
paused.update({"paused": True, "state": "paused", "stateLabel": "Paused"})
scenarios["paused"] = paused

idle = copy.deepcopy(payload)
idle.update({
    "state": "idle",
    "title": "Nothing playing",
    "artist": "YuTuTui!",
    "stateLabel": "Idle",
    "ownerLabel": "Daemon",
    "queueLabel": "Queue empty",
    "volume": 50,
    "elapsedMs": None,
    "durationMs": None,
    "trackIdentity": "v8\\u001fidle\\u001f8",
    "canSeek": False,
    "queue": [],
    "paused": True,
    "streaming": False,
    "canPlayback": False,
    "canResumeDaemon": True,
    "canStopDaemon": True,
})
scenarios["idle"] = idle

disconnected = copy.deepcopy(payload)
disconnected.update({
    "connected": False,
    "state": "disconnected",
    "title": "YuTuTui! is not running",
    "artist": "YuTuTray!",
    "stateLabel": "Disconnected",
    "ownerLabel": "Offline",
    "queueLabel": "Queue unavailable",
    "volume": 0,
    "elapsedMs": None,
    "durationMs": None,
    "isLive": False,
    "queueRev": None,
    "trackIdentity": "disconnected",
    "canSeek": False,
    "queue": [],
    "paused": True,
    "streaming": False,
    "error": "YuTuTui! is not running",
    "canPlayback": False,
    "canVolume": False,
    "canManageQueue": False,
    "canToggleStreaming": False,
    "canStartDaemon": True,
    "canResumeDaemon": True,
    "canStopDaemon": False,
})
scenarios["disconnected"] = disconnected

live = copy.deepcopy(payload)
live.update({
    "title": "Seoul Night Radio",
    "elapsedMs": 42000,
    "durationMs": None,
    "isLive": True,
    "trackIdentity": "v8\\u001flive-track\\u001f9",
    "canSeek": False,
})
scenarios["live"] = live

pending = copy.deepcopy(payload)
pending.update({
    "stateLabel": "Applying setting…",
    "error": None,
})
scenarios["pending"] = pending

rejected = copy.deepcopy(payload)
rejected.update({
    "error": "Autoplay and repeat cannot be enabled at the same time.",
})
scenarios["rejected"] = rejected

art_missing = copy.deepcopy(payload)
art_missing.update({
    "title": "Artwork unavailable",
    "trackIdentity": "v8\\u001fmissing-art\\u001f10",
})
scenarios["art-missing"] = art_missing

long_text = copy.deepcopy(payload)
long_text.update({
    "title": "긴게 번역된 한국어 곡 제목과 emoji 🌙 ونص عربي 혼합 표시를 검증하는 트랙",
    "artist": "아티스트 이름이 아주 길 때의 안전한 줄임 검증",
    "trackIdentity": "1\\u001flong-text\\u001fartist\\u001f180000\\u001f2",
    "error": "A deliberately long recoverable command error that must truncate without covering controls",
})
long_text["queue"][0]["title"] = long_text["title"]
long_text["queue"][0]["artist"] = long_text["artist"]
scenarios["long-text"] = long_text

long_queue = copy.deepcopy(payload)
long_queue["queue"] = [
    {
        "index": index,
        "title": f"Queue item {index + 1}: a title that tests safe truncation",
        "artist": "Fixture Artist",
        "duration": f"{2 + index // 10}:{index % 60:02d}",
        "current": index == 7,
    }
    for index in range(30)
]
long_queue.update({
    "queueLabel": "8 / 30",
    "queueRev": 42,
    "trackIdentity": "v8\\u001flong-queue\\u001f11",
})
scenarios["long-queue"] = long_queue

# Keep in sync with PanelTheme::window_size (src/desktop/panel.rs).
sizes = {"default": (398, 602), "minimal": (306, 90), "tamagotchi": (290, 346)}

for theme in sizes:
    for scenario, fixture in scenarios.items():
        page = (
            src.replace("__INITIAL_PAYLOAD__", json.dumps(fixture))
            .replace("__PANEL_THEME__", theme)
            .replace("__PANEL_LANG__", "en")
            .replace("__PANEL_LOCALE__", "en")
            .replace("__INITIAL_ART__", "null")
            .replace("__INITIAL_PINNED__", "false")
            .replace("__INITIAL_EXPANDED__", "false")
            .replace("__INITIAL_SHARED_SHEET__", "null")
            .replace("__INITIAL_QUEUE_SCROLL_Y__", "0")
            .replace("__INITIAL_ACTIVE_CONTROL__", "null")
            .replace("__CSP_NONCE__", "preview-nonce")
        )
        fixture_script = ""
        if scenario == "pending":
            fixture_script = """
  <script nonce="preview-nonce">
    const pendingFixture = document.getElementById("shuffle");
    pendingFixture.classList.add("pending");
    pendingFixture.setAttribute("aria-busy", "true");
    pendingFixture.setAttribute("aria-disabled", "true");
  </script>
"""
        elif scenario == "long-queue":
            # Queue is the evidence under review. The compact skins open their shared
            # 398×602 work surface; Cushion selects the same semantic panel in place.
            fixture_script = """
  <script nonce="preview-nonce">
    document.getElementById("tabQueue").click();
  </script>
"""
        page = page.replace("</body>", fixture_script + "</body>")
        (out / f"panel-{theme}-{scenario}.html").write_text(page)
        if scenario == "playing":
            # Stable convenience path retained for existing preview instructions.
            (out / f"panel-{theme}.html").write_text(page)

frames = "\n".join(
    f'<figure><figcaption>{theme} / {scenario} — '
    f'{398 if scenario == "long-queue" else w}×{602 if scenario == "long-queue" else h}</figcaption>'
    f'<iframe src="panel-{theme}-{scenario}.html" width="{398 if scenario == "long-queue" else w}" '
    f'height="{602 if scenario == "long-queue" else h}" title="{theme} {scenario}"></iframe></figure>'
    for scenario in scenarios
    for theme, (w, h) in sizes.items()
)
(out / "preview.html").write_text(f"""<!doctype html>
<html><head><meta charset="utf-8"><title>panel preview</title><style>
  /* Match the page's color-scheme: a mismatch makes browsers paint the iframes
     onto an opaque canvas, hiding the transparency we're here to check. */
  :root {{ color-scheme: dark; }}
  body {{ margin: 0; padding: 24px; display: flex; gap: 24px; align-items: flex-start; flex-wrap: wrap;
         background: repeating-conic-gradient(#c8c8c8 0% 25%, #efefef 0% 50%) 0 0 / 22px 22px; }}
  figure {{ margin: 0; }}
  figcaption {{ font: 12px system-ui; margin-bottom: 6px; background: #fff8; padding: 2px 6px;
                border-radius: 6px; display: inline-block; }}
  iframe {{ border: 0; display: block; }}
</style></head><body>
{frames}
</body></html>
""")
print(out / "preview.html")
PY
