//! Mouse-binding helpers for the Settings Keys tab.

use super::*;
use crate::mousemap::{self, MouseBindingError, MouseContext, MouseGesture};

impl App {
    /// The mouse binding under the Keys-tab cursor. Mouse rows follow all keyboard rows in
    /// stable context/gesture order so rendering, navigation, and hit targets share one index.
    pub(in crate::app) fn settings_current_mouse_binding(
        &self,
    ) -> Option<(MouseContext, MouseGesture)> {
        let st = self.settings.as_ref()?;
        if st.tab != SettingsTab::Keys {
            return None;
        }
        let offset = st
            .row
            .checked_sub(crate::keymap::editable_entries().len())?;
        let gestures = MouseGesture::ALL.len();
        let context = MouseContext::ALL.get(offset / gestures)?;
        let gesture = MouseGesture::ALL.get(offset % gestures)?;
        Some((*context, *gesture))
    }

    /// Step the selected gesture through safe preset actions. Conflicting single/double-click
    /// choices are rejected by the binding model and surfaced without mutating the draft.
    pub(in crate::app) fn settings_change_mouse_binding(&mut self, dir: i32) {
        let Some((context, gesture)) = self.settings_current_mouse_binding() else {
            return;
        };
        let Some(st) = self.settings.as_mut() else {
            return;
        };
        let actions = mousemap::allowed_actions(context, gesture);
        let current = st.mousemap.action(context, gesture);
        let current_index = actions
            .iter()
            .position(|action| *action == current)
            .unwrap_or(0);
        let next_index =
            (current_index as i32 + dir.signum()).rem_euclid(actions.len() as i32) as usize;
        let next = actions[next_index];
        match st.mousemap.set(context, gesture, next) {
            Ok(()) => {
                self.status.kind = StatusKind::Info;
                self.status.text = format!(
                    "{} · {} → {}",
                    context.title(),
                    gesture.human_label(),
                    next.human_label()
                );
            }
            Err(MouseBindingError::DirectRightClickConflict { .. }) => {
                self.status.kind = StatusKind::Error;
                self.status.text = t!(
                    "Set right click to Context menu or Disabled before enabling right double-click",
                    "우더블클릭을 켜려면 먼저 우클릭을 문맥 메뉴 또는 사용 안 함으로 바꾸세요",
                    "先に右クリックをコンテキストメニューまたは無効に変更してください"
                )
                .to_owned();
            }
            Err(error) => {
                self.status.kind = StatusKind::Error;
                self.status.text = error.to_string();
            }
        }
        self.dirty = true;
    }

    /// Reset the mouse row under the cursor. Returns whether the cursor was on a mouse row so
    /// the shared reset entry point knows whether to continue with keyboard bindings.
    pub(in crate::app) fn settings_reset_mouse_binding(&mut self) -> bool {
        let Some((context, gesture)) = self.settings_current_mouse_binding() else {
            return false;
        };
        if let Some(st) = self.settings.as_mut() {
            match st.mousemap.reset_binding(context, gesture) {
                Ok(()) => {
                    self.status.kind = StatusKind::Info;
                    self.status.text = match crate::i18n::current() {
                        crate::i18n::Language::Korean => format!(
                            "{} · {} 을(를) 기본값으로 되돌림",
                            context.title(),
                            gesture.human_label()
                        ),
                        crate::i18n::Language::Japanese => format!(
                            "{} · {} をデフォルトに戻しました",
                            context.title(),
                            gesture.human_label()
                        ),
                        _ => format!(
                            "Reset {} · {} to default",
                            context.title(),
                            gesture.human_label()
                        ),
                    };
                }
                Err(MouseBindingError::DirectRightClickConflict { .. }) => {
                    self.status.kind = StatusKind::Error;
                    self.status.text = t!(
                        "Reset right click first, then reset right double-click",
                        "먼저 우클릭을 초기화한 뒤 우더블클릭을 초기화하세요",
                        "先に右クリックを、次に右ダブルクリックをリセットしてください"
                    )
                    .to_owned();
                }
                Err(error) => {
                    self.status.kind = StatusKind::Error;
                    self.status.text = error.to_string();
                }
            }
            self.dirty = true;
        }
        true
    }
}
