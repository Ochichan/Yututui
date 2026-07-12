# Desktop native window-role QA

Status: acceptance procedure only. This file does not claim that any Windows or macOS
check has run. Record the commit, binary path, OS build, displays, and evidence before
marking a row as passed.

## Activation intents

Start one primary process with `yututray --background`, then invoke each command as a
secondary process and confirm that the secondary exits without creating another tray icon.

| Invocation | Primary startup | Existing primary |
|---|---|---|
| `yututray` | tray only | show/focus the mini player |
| `yututray --background` | tray only | raise no window |
| `yututray --mini` | tray + mini player | show/focus the mini player |
| `yututray --main-window` | tray + main window | show/focus the main window |

On Windows, one left-click toggles the mini player and right-click opens the native menu.
On macOS, use the native status-item menu to show the mini player.

## Windows 10/11

Use `scripts/windows-tray-manual-qa.ps1` to collect evidence and
`scripts/verify-windows-tray-manual-qa.ps1` to validate its shape. The verifier validates
answers and artifacts; it cannot prove that an operator inspected the correct HWND.

1. Inspect the visible `YuTuTray! Mini Player` top-level HWND with Spy++ or WinDbg.
   `GWL_EXSTYLE` must have `WS_EX_TOOLWINDOW` (`0x00000080`) and must not have
   `WS_EX_APPWINDOW` (`0x00040000`). The window must remain keyboard-focusable when
   clicked, but be absent from the taskbar and Alt-Tab.
2. Inspect the decorated `YuTuTray!` main HWND. It must have `WS_EX_APPWINDOW`, must not
   have `WS_EX_TOOLWINDOW`, and must appear exactly once in the taskbar and Alt-Tab.
3. Hide the main window. Its taskbar and Alt-Tab entries must disappear immediately while
   the tray process remains. The mini player must remain excluded throughout.
4. Repeat at 100%, 150%, and 200% scale, across mixed-DPI monitors, and after restarting
   Explorer. Capture task-switcher state separately from visual-crispness screenshots.

## macOS

Capture the Dock and Cmd-Tab state at each step; Activity Monitor process presence alone is
not evidence of activation policy.

1. `--background`: activation policy is `Accessory`; no Dock icon or Cmd-Tab entry.
2. Mini player visible or pinned: policy remains `Accessory`; still no Dock/Cmd-Tab entry.
3. Main window visible: policy becomes `Regular`; one Dock icon and one Cmd-Tab entry; the
   existing main window is focused by subsequent `--main-window` invocations.
4. Last main window hidden: policy returns to `Accessory`; Dock/Cmd-Tab entries disappear
   without terminating the tray process.
5. Repeat across Spaces, beside a fullscreen Space, and after sleep/wake.

Do not report either platform as verified until its native evidence has been reviewed.
