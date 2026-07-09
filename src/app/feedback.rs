//! Status-line and zoom feedback helpers.

use super::*;

impl App {
    /// Whether a transient status is currently covering the title (drives the main loop's
    /// expiry tick - see [`Msg::StatusTick`]).
    pub fn status_visible(&self) -> bool {
        self.status.set_at.is_some()
    }

    pub(crate) fn set_status_info(&mut self, text: impl Into<String>) {
        self.status.kind = StatusKind::Info;
        self.status.text = text.into();
        self.dirty = true;
    }

    pub(crate) fn set_status_error(&mut self, text: impl Into<String>) {
        self.status.kind = StatusKind::Error;
        self.status.text = text.into();
        self.dirty = true;
    }

    pub(crate) fn clear_status(&mut self) {
        self.status.kind = StatusKind::Error;
        self.status.text.clear();
        self.dirty = true;
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
