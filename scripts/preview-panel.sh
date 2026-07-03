#!/usr/bin/env bash
# Dev-only preview of the mini player page (src/desktop/panel.html) outside the
# tray: replaces the host-spliced tokens with a dummy payload and opens every
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
import json
import pathlib
import sys

out = pathlib.Path(sys.argv[1])
src = pathlib.Path("src/desktop/panel.html").read_text()

# A playing snapshot exercising every field the page reads.
payload = {
    "connected": True,
    "title": "Longer Song Title For Marquee Testing",
    "artist": "Fox Artist",
    "stateLabel": "Playing",
    "ownerLabel": "Daemon",
    "queueLabel": "1 / 2",
    "volumeLabel": "80%",
    "volume": 80,
    "elapsedMs": 42000,
    "durationMs": 180000,
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

# Keep in sync with PanelTheme::window_size (src/desktop/panel.rs).
sizes = {"default": (398, 602), "minimal": (306, 90), "tamagotchi": (290, 346)}

for theme in sizes:
    page = (
        src.replace("__INITIAL_PAYLOAD__", json.dumps(payload))
        .replace("__PANEL_THEME__", theme)
        .replace("__INITIAL_ART__", "null")
    )
    (out / f"panel-{theme}.html").write_text(page)

frames = "\n".join(
    f'<figure><figcaption>{theme} — {w}×{h}</figcaption>'
    f'<iframe src="panel-{theme}.html" width="{w}" height="{h}"></iframe></figure>'
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
