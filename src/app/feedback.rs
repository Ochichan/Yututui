//! Status-line and zoom feedback helpers.

use std::time::Instant;

use super::*;

impl App {
    /// Whether a transient status is currently covering the title (drives the main loop's
    /// expiry tick - see [`Msg::StatusTick`]).
    pub fn status_visible(&self) -> bool {
        self.status.set_at.is_some() || self.onboarding.visible()
    }

    pub(crate) fn set_status_info(&mut self, text: impl Into<String>) {
        self.status.kind = StatusKind::Info;
        self.status.text = text.into();
        self.status.set_at = (!self.status.text.is_empty()).then(Instant::now);
        self.dirty = true;
    }

    pub(crate) fn set_status_error(&mut self, text: impl Into<String>) {
        self.status.kind = StatusKind::Error;
        self.status.text = text.into();
        self.status.set_at = (!self.status.text.is_empty()).then(Instant::now);
        self.dirty = true;
    }

    pub(crate) fn clear_status(&mut self) {
        self.status.kind = StatusKind::Error;
        self.status.text.clear();
        self.status.set_at = None;
        self.dirty = true;
    }

    /// One-shot hygiene when the render pass crosses into the miniplayer tier: drop
    /// text-entry focus so the invisible inputs can't eat the transport keys (the
    /// mode-owned modals keep their routing — see `mini_mode_owns_modal`). Uncommitted
    /// Settings text editing is cancelled (the draft keeps its previous value). Surfaces
    /// the miniplayer *does* render — the queue window and the status-line dropdowns —
    /// stay open and operable.
    pub(in crate::app) fn sync_ui_tier(&mut self) {
        let tier = self.bridges.ui_tier.get();
        if tier != self.fx.last_ui_tier {
            self.fx.last_ui_tier = tier;
            self.dirty = true;
        }
        if tier != crate::ui::layout::UiTier::Mini {
            return;
        }
        // Level-triggered, not edge-triggered: navigation inside the miniplayer re-focuses
        // inputs (`s` → Search focuses its box), which would re-suppress the typeable
        // globals the entry pass freed. The resets are cheap and idempotent.
        if self.search.focus == SearchFocus::Input {
            self.search.focus = SearchFocus::Results;
            self.search.select_all = false;
            self.dirty = true;
        }
        if self.ai.focus == AiFocus::Input {
            self.ai.focus = AiFocus::Suggestions;
            self.dirty = true;
        }
        // The results-filter popup renders only inside the (suppressed) Search view and its
        // key capture runs before the mini routing guard — leaving it open would recreate
        // the invisible-modal trap this hygiene exists to prevent.
        if self.search_filter.open {
            self.search_filter.close();
            self.dirty = true;
        }
        self.library_ui.filter_editing = false;
        self.local_mode.ui.filter_editing = false;
        if let Some(s) = self.settings.as_mut() {
            s.editing_text = false;
        }
    }

    /// Apply the backend's actual logical grid tier before drawing. Text zoom can cross the Mini
    /// boundary without a terminal resize event, so waiting for the render bridge's next reducer
    /// turn would show one stale paused-lyric frame and start a pending OSD late.
    pub(crate) fn prepare_ui_tier_for_render(&mut self, tier: crate::ui::layout::UiTier) {
        if self.bridges.ui_tier.replace(tier) != tier {
            let commands = self.update(Msg::Noop);
            debug_assert!(commands.is_empty());
        }
    }

    /// Collapse or expand the docked control box on non-Player screens (Bottom bar mode
    /// only — in the Top layout there is nothing to collapse, and the Player screen always
    /// shows its controls). Shared by the `B` shortcut and the ▲/▼ footer button. Persists
    /// like every other Settings-backed preference and shows a transient toast.
    pub(in crate::app) fn toggle_control_box(&mut self) -> Vec<Cmd> {
        if self.player_bar_position() != crate::config::PlayerBarPosition::Bottom {
            self.set_status_info(
                t!(
                    "Player bar is in the classic Top layout — nothing to collapse",
                    "플레이어 바가 클래식 상단 배치예요 — 접을 것이 없어요"
                )
                .to_owned(),
            );
            return Vec::new();
        }
        let collapsed = !self.config.control_box_collapsed();
        self.config.control_box_collapsed = Some(collapsed);
        if collapsed && self.mode != Mode::Player {
            self.dropdowns.eq_open = false;
            self.dropdowns.streaming_open = false;
        }
        // The box moves/vanishes under native art only via screen switches (it never shows
        // on Player), but the Player-screen rect is unaffected — no native clear needed.
        self.set_status_info(if collapsed {
            t!(
                "Player bar collapsed on other screens (B to expand)",
                "다른 화면에서 플레이어 바를 접었어요 (B로 펼치기)"
            )
            .to_owned()
        } else {
            t!("Player bar expanded", "플레이어 바를 펼쳤어요").to_owned()
        });
        self.dirty = true;
        vec![Cmd::Persist(PersistCmd::Config(Box::new(
            self.config.clone(),
        )))]
    }

    /// Step the text zoom one notch up or down (Ctrl+wheel / Ctrl+-/=). On terminals
    /// without the text sizing protocol this explains itself in a toast instead of
    /// silently doing nothing - the keys are advertised in the cheat-sheet, so a dead
    /// key would read as a bug.
    pub(in crate::app) fn zoom_step(&mut self, zoom_in: bool) -> Vec<Cmd> {
        if !self.zoom.supported() {
            self.set_status_info(t!(
                "This terminal can't scale text (kitty 0.40+, Windows Terminal, …)",
                "이 터미널은 글자 확대를 지원하지 않아요 (kitty 0.40+, Windows Terminal 등 가능)"
            )
            .to_owned());
            return Vec::new();
        }
        let current = self.zoom.percent();
        let next = self.zoom.step(zoom_in);
        if next == current {
            let status = if zoom_in {
                let max = self.zoom.max_percent();
                if crate::i18n::is_korean() {
                    format!("이미 최대 글자 크기예요 ({max}%)")
                } else {
                    format!("Text is already at its largest ({max}%)")
                }
            } else {
                t!(
                    "Text is back to its normal size (100%)",
                    "기본 글자 크기예요 (100%)"
                )
                .to_owned()
            };
            self.set_status_info(status);
            return Vec::new();
        }
        self.zoom.set(next);
        let status = if crate::i18n::is_korean() {
            format!("글자 크기 {next}%")
        } else {
            format!("Text size {next}%")
        };
        self.set_status_info(status);
        // The virtual grid just changed size: force the full VT-clear redraw path so no
        // scaled multicells (or native-image placements) from the old grid survive.
        self.request_native_image_clear();
        self.config.text_zoom = Some(next);
        vec![Cmd::Persist(PersistCmd::Config(Box::new(
            self.config.clone(),
        )))]
    }

    /// Toggle the Ctrl+wheel zoom lock (`ToggleZoomWheelLock`). Persisted, so a
    /// deliberately frozen gesture stays frozen across sessions.
    pub(in crate::app) fn toggle_zoom_wheel_lock(&mut self) -> Vec<Cmd> {
        let locked = !self.config.effective_zoom_wheel_lock();
        self.config.zoom_wheel_lock = Some(locked);
        self.status.kind = StatusKind::Info;
        self.status.text = if locked {
            t!(
                "Ctrl+wheel zoom locked (Ctrl+-/= still zoom)",
                "Ctrl+휠 확대 잠금 켜짐 (Ctrl+-/= 는 그대로 동작)"
            )
        } else {
            t!("Ctrl+wheel zoom unlocked", "Ctrl+휠 확대 잠금 꺼짐")
        }
        .to_owned();
        self.dirty = true;
        vec![Cmd::Persist(PersistCmd::Config(Box::new(
            self.config.clone(),
        )))]
    }
}
