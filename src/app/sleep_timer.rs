//! Sleep-timer reducer: the popup, arming, the 1 Hz fade tick, and the fire path.
//!
//! The timer state machine itself lives in `yututui_core::sleep_timer` so the App and the
//! headless daemon apply identical fade/fire semantics; this module only translates its
//! [`SleepStep`]s into the App's canonical `player_intent` paths (SetVolume / pause) and
//! feeds the popup.

use super::*;
use yututui_core::sleep_timer::{SLEEP_MAX_MINUTES, SleepStep, SleepTimer};

/// Grouped sleep-timer state: the armed timer (the shared core machine) and its popup.
/// Kept as one sub-struct so the flat `App` stays within the architecture gate's field
/// budget; both halves belong to the same feature and reset together.
#[derive(Debug, Default)]
pub struct SleepState {
    pub timer: Option<SleepTimer>,
    pub popup: Option<SleepPopup>,
}

/// The open sleep-timer popup (minutes or `off`).
#[derive(Debug, Default)]
pub struct SleepPopup {
    pub input: String,
    pub cursor: TextCursor,
    /// The last commit failed to parse; the popup reopens showing the hint.
    pub error: bool,
    /// The input still holds the untouched preset — the first typed digit replaces it
    /// instead of appending (`30` + `1` must read `1`, not `301`).
    pub untouched: bool,
}

impl App {
    /// Whether the terminal runner should arm the 1 Hz sleep tick.
    pub fn sleep_timer_active(&self) -> bool {
        self.sleep.timer.is_some()
    }

    /// Open the popup pre-filled with the configured preset.
    pub(in crate::app) fn open_sleep_popup(&mut self) {
        let preset = self.config.sleep_timer.effective_default_minutes();
        let input = preset.to_string();
        self.sleep.popup = Some(SleepPopup {
            cursor: TextCursor::at_end(&input),
            input,
            error: false,
            untouched: true,
        });
        self.dirty = true;
    }

    /// Keystrokes while the sleep popup is open: digits edit the minutes, Enter commits,
    /// Esc cancels. Mirrors the create-playlist popup's plain-char gate.
    pub(in crate::app) fn on_key_sleep_popup(&mut self, k: KeyEvent) -> Vec<Cmd> {
        if let Some(action) = self.keymap.text_edit_action(k.into()) {
            if let Some(popup) = self.sleep.popup.as_mut()
                && let Some(result) =
                    apply_text_edit_action(action, &mut popup.cursor, &mut popup.input)
                && matches!(
                    result,
                    TextEditResult::BufferChanged(true) | TextEditResult::CursorMoved(true)
                )
            {
                popup.untouched = false;
                self.dirty = true;
            }
            return Vec::new();
        }
        match k.code {
            KeyCode::Esc => {
                self.sleep.popup = None;
                self.dirty = true;
            }
            KeyCode::Enter => return self.commit_sleep_popup(),
            KeyCode::Char(c)
                if !k
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                if let Some(popup) = self.sleep.popup.as_mut() {
                    // The first typed character replaces the untouched preset, so both
                    // `1` (minutes) and `o` (the start of "off") read naturally.
                    if popup.untouched {
                        popup.input.clear();
                        popup.cursor = TextCursor::default();
                        popup.untouched = false;
                    }
                    let lower = c.to_ascii_lowercase();
                    let fits_word = match popup.input.trim().to_ascii_lowercase().as_str() {
                        "" => lower == 'o',
                        "o" => lower == 'f',
                        "of" => lower == 'f',
                        _ => false,
                    };
                    let fits_digit = c.is_ascii_digit()
                        && popup.input.len() < 4
                        && popup.input.chars().all(|ch| ch.is_ascii_digit());
                    if fits_word || fits_digit {
                        popup.input.push(c);
                        popup.cursor = TextCursor::at_end(&popup.input);
                        self.dirty = true;
                    }
                }
            }
            _ => {}
        }
        Vec::new()
    }

    /// Commit the popup: `off`/empty/`0` cancels, a minute count arms the timer, anything
    /// else reopens the popup with the error hint.
    pub(in crate::app) fn commit_sleep_popup(&mut self) -> Vec<Cmd> {
        let popup = self
            .sleep
            .popup
            .take()
            .expect("commit requires an open popup");
        self.dirty = true;
        let input = popup.input.trim();
        if input.is_empty() || input.eq_ignore_ascii_case("off") {
            return self.cancel_sleep_timer();
        }
        match input.parse::<u32>() {
            Ok(0) => self.cancel_sleep_timer(),
            Ok(minutes) => self.arm_sleep_timer(minutes),
            Err(_) => {
                self.sleep.popup = Some(SleepPopup {
                    input: input.to_string(),
                    cursor: TextCursor::default(),
                    error: true,
                    untouched: false,
                });
                Vec::new()
            }
        }
    }

    /// Arm the timer through the shared core state machine.
    pub(in crate::app) fn arm_sleep_timer(&mut self, minutes: u32) -> Vec<Cmd> {
        let minutes = minutes.clamp(1, SLEEP_MAX_MINUTES);
        let fade_secs = self.config.sleep_timer.effective_fade_secs();
        self.sleep.timer = Some(SleepTimer::armed(Instant::now(), minutes, fade_secs));
        let label = t!(
            "Sleep timer set:",
            "수면 타이머 설정:",
            "スリープタイマー設定:"
        );
        let unit = t!("min", "분", "分");
        self.set_status_info(format!("{label} {minutes} {unit}"));
        self.dirty = true;
        Vec::new()
    }

    /// Cancel the timer; a fade already in progress restores its pre-fade volume through
    /// the canonical SetVolume path.
    pub(in crate::app) fn cancel_sleep_timer(&mut self) -> Vec<Cmd> {
        let mut cmds = Vec::new();
        if let Some(timer) = self.sleep.timer
            && timer.fading
            && let Some(pre) = timer.pre_fade_volume
            && self.playback.volume != pre
        {
            cmds = self.player_intent(
                "sleep_restore",
                PlayerCmd::SetVolume(pre),
                PlayerCommit::Volume {
                    volume: pre,
                    pre_mute_volume: None,
                },
            );
        }
        self.sleep.timer = None;
        self.set_status_info(t!(
            "Sleep timer off",
            "수면 타이머 꺼짐",
            "スリープタイマーをオフにしました"
        ));
        self.dirty = true;
        cmds
    }

    /// One 1 Hz tick while a timer is armed: advance the fade, or fire at the deadline.
    pub(in crate::app) fn handle_sleep_tick(&mut self) -> Vec<Cmd> {
        let Some(mut timer) = self.sleep.timer else {
            return Vec::new();
        };
        let now = Instant::now();
        match timer.advance(now, self.playback.volume) {
            SleepStep::Idle => {
                self.sleep.timer = Some(timer);
                self.dirty = true;
                Vec::new()
            }
            SleepStep::Volume(volume) => {
                self.sleep.timer = Some(timer);
                self.player_intent(
                    "sleep_fade",
                    PlayerCmd::SetVolume(volume),
                    PlayerCommit::Volume {
                        volume,
                        pre_mute_volume: None,
                    },
                )
            }
            SleepStep::Fired => {
                let restore = timer.pre_fade_volume;
                self.sleep.timer = None;
                let mut cmds = Vec::new();
                if let Some(pre) = restore {
                    cmds.extend(self.player_intent(
                        "sleep_restore",
                        PlayerCmd::SetVolume(pre),
                        PlayerCommit::Volume {
                            volume: pre,
                            pre_mute_volume: None,
                        },
                    ));
                }
                cmds.extend(self.player_intent(
                    "sleep_fire",
                    PlayerCmd::SetProperty {
                        name: "pause".to_owned(),
                        value: serde_json::Value::Bool(true),
                    },
                    PlayerCommit::Pause {
                        paused: true,
                        clear_video_pause: false,
                    },
                ));
                self.set_status_info(t!(
                    "Sleep timer done — playback paused",
                    "수면 타이머 종료 — 재생이 일시정지되었습니다",
                    "スリープタイマー終了 — 再生を一時停止しました"
                ));
                self.dirty = true;
                cmds
            }
        }
    }

    /// The remote reply line for a successful arm: the remaining time in `mm:ss`.
    pub(in crate::app) fn sleep_timer_resp(&self) -> crate::remote::proto::RemoteResponse {
        let remaining = self
            .sleep
            .timer
            .and_then(|timer| timer.remaining_secs(Instant::now()))
            .unwrap_or(0);
        crate::remote::proto::RemoteResponse::ok(format!(
            "sleep timer: {}:{:02}",
            remaining / 60,
            remaining % 60
        ))
    }
}
