//! Persisted mouse gesture bindings.
//!
//! Mouse bindings intentionally form a smaller command surface than keyboard bindings:
//! destructive and modal actions stay behind the context menu, while the two right-button
//! gestures may open that menu, activate a row, enqueue it, or do nothing. Config stores only
//! deviations from the built-in defaults, matching [`crate::keymap`]'s forward-compatible
//! persistence model.

use std::collections::BTreeMap;
use std::fmt;

/// TUI surface whose rows receive a mouse gesture.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum MouseContext {
    Search,
    Library,
    Queue,
    Local,
}

impl MouseContext {
    pub const ALL: [Self; 4] = [Self::Search, Self::Library, Self::Queue, Self::Local];

    /// Stable identifier used in `config.json` keys.
    pub const fn id(self) -> &'static str {
        match self {
            Self::Search => "search",
            Self::Library => "library",
            Self::Queue => "queue",
            Self::Local => "local",
        }
    }

    pub fn from_id(id: &str) -> Option<Self> {
        match id {
            "search" => Some(Self::Search),
            "library" => Some(Self::Library),
            "queue" => Some(Self::Queue),
            "local" => Some(Self::Local),
            _ => None,
        }
    }

    /// Human-readable group label for the future Settings editor.
    pub fn title(self) -> &'static str {
        match self {
            Self::Search => crate::t!("Search", "검색", "検索"),
            Self::Library => crate::t!("Library", "라이브러리", "ライブラリ"),
            Self::Queue => crate::t!("Queue", "대기열", "キュー"),
            Self::Local => crate::t!("Local Deck", "로컬 덱", "ローカルデッキ"),
        }
    }
}

/// Right-button gesture that can be assigned independently.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum MouseGesture {
    RightClick,
    RightDoubleClick,
}

impl MouseGesture {
    pub const ALL: [Self; 2] = [Self::RightClick, Self::RightDoubleClick];

    pub const fn id(self) -> &'static str {
        match self {
            Self::RightClick => "right_click",
            Self::RightDoubleClick => "right_double_click",
        }
    }

    pub fn from_id(id: &str) -> Option<Self> {
        match id {
            "right_click" => Some(Self::RightClick),
            "right_double_click" => Some(Self::RightDoubleClick),
            _ => None,
        }
    }

    pub fn human_label(self) -> &'static str {
        match self {
            Self::RightClick => crate::t!("Right click", "우클릭", "右クリック"),
            Self::RightDoubleClick => {
                crate::t!("Right double-click", "우더블클릭", "右ダブルクリック")
            }
        }
    }

    pub const fn default_action(self) -> MouseAction {
        match self {
            Self::RightClick => MouseAction::ContextMenu,
            Self::RightDoubleClick => MouseAction::Activate,
        }
    }
}

/// Safe command exposed to mouse gesture configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum MouseAction {
    ContextMenu,
    Activate,
    Enqueue,
    Disabled,
}

impl MouseAction {
    pub const ALL: [Self; 4] = [
        Self::ContextMenu,
        Self::Activate,
        Self::Enqueue,
        Self::Disabled,
    ];

    pub const fn id(self) -> &'static str {
        match self {
            Self::ContextMenu => "context_menu",
            Self::Activate => "activate",
            Self::Enqueue => "enqueue",
            Self::Disabled => "disabled",
        }
    }

    pub fn from_id(id: &str) -> Option<Self> {
        match id {
            "context_menu" => Some(Self::ContextMenu),
            "activate" => Some(Self::Activate),
            "enqueue" => Some(Self::Enqueue),
            "disabled" => Some(Self::Disabled),
            _ => None,
        }
    }

    pub fn human_label(self) -> &'static str {
        match self {
            Self::ContextMenu => crate::t!("Context menu", "문맥 메뉴", "コンテキストメニュー"),
            Self::Activate => crate::t!("Activate", "실행", "実行"),
            Self::Enqueue => crate::t!("Add to queue", "대기열에 추가", "キューに追加"),
            Self::Disabled => crate::t!("Disabled", "사용 안 함", "無効"),
        }
    }

    const fn is_direct_single_action(self) -> bool {
        matches!(self, Self::Activate | Self::Enqueue)
    }
}

const RIGHT_CLICK_ACTIONS: &[MouseAction] = &[
    MouseAction::ContextMenu,
    MouseAction::Activate,
    MouseAction::Enqueue,
    MouseAction::Disabled,
];
const QUEUE_RIGHT_CLICK_ACTIONS: &[MouseAction] = &[
    MouseAction::ContextMenu,
    MouseAction::Activate,
    MouseAction::Disabled,
];
const RIGHT_DOUBLE_CLICK_ACTIONS: &[MouseAction] = &[
    MouseAction::Activate,
    MouseAction::Enqueue,
    MouseAction::Disabled,
];
const QUEUE_RIGHT_DOUBLE_CLICK_ACTIONS: &[MouseAction] =
    &[MouseAction::Activate, MouseAction::Disabled];

/// Actions valid for a context/gesture pair, in Settings picker order.
pub const fn allowed_actions(
    context: MouseContext,
    gesture: MouseGesture,
) -> &'static [MouseAction] {
    match (context, gesture) {
        (MouseContext::Queue, MouseGesture::RightClick) => QUEUE_RIGHT_CLICK_ACTIONS,
        (MouseContext::Queue, MouseGesture::RightDoubleClick) => QUEUE_RIGHT_DOUBLE_CLICK_ACTIONS,
        (_, MouseGesture::RightClick) => RIGHT_CLICK_ACTIONS,
        (_, MouseGesture::RightDoubleClick) => RIGHT_DOUBLE_CLICK_ACTIONS,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseBindingError {
    ActionNotAllowed {
        context: MouseContext,
        gesture: MouseGesture,
        action: MouseAction,
    },
    DirectRightClickConflict {
        context: MouseContext,
        right_click: MouseAction,
    },
}

impl fmt::Display for MouseBindingError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ActionNotAllowed {
                context,
                gesture,
                action,
            } => write!(
                f,
                "mouse action {} is not allowed for {}.{}",
                action.id(),
                context.id(),
                gesture.id()
            ),
            Self::DirectRightClickConflict {
                context,
                right_click,
            } => write!(
                f,
                "{}.right_double_click must stay disabled while right_click is {}",
                context.id(),
                right_click.id()
            ),
        }
    }
}

impl std::error::Error for MouseBindingError {}

/// Fully resolved mouse bindings. Every context/gesture pair always has an action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MouseMap {
    bindings: BTreeMap<(MouseContext, MouseGesture), MouseAction>,
}

impl Default for MouseMap {
    fn default() -> Self {
        let mut bindings = BTreeMap::new();
        for context in MouseContext::ALL {
            for gesture in MouseGesture::ALL {
                bindings.insert((context, gesture), gesture.default_action());
            }
        }
        Self { bindings }
    }
}

impl MouseMap {
    /// Layer persisted `"<context>.<gesture>" -> "<action>"` overrides over defaults.
    /// Unknown, malformed, disallowed, and internally conflicting entries are ignored.
    pub fn from_overrides(overrides: &BTreeMap<String, String>) -> Self {
        let mut map = Self::default();
        for (key, value) in overrides {
            let Some((context_id, gesture_id)) = key.split_once('.') else {
                tracing::warn!(key, "ignoring malformed mouse binding override");
                continue;
            };
            let Some(context) = MouseContext::from_id(context_id) else {
                tracing::warn!(key, value, "ignoring unknown mouse binding override");
                continue;
            };
            let Some(gesture) = MouseGesture::from_id(gesture_id) else {
                tracing::warn!(key, value, "ignoring unknown mouse binding override");
                continue;
            };
            let Some(action) = MouseAction::from_id(value) else {
                tracing::warn!(key, value, "ignoring unknown mouse binding override");
                continue;
            };
            if let Err(error) = map.set(context, gesture, action) {
                tracing::warn!(key, value, %error, "ignoring invalid mouse binding override");
            }
        }
        map
    }

    pub fn from_config(config: &crate::config::Config) -> Self {
        Self::from_overrides(&config.mouse_bindings)
    }

    /// Resolve a gesture. All combinations are populated, so this never returns `None`.
    pub fn action(&self, context: MouseContext, gesture: MouseGesture) -> MouseAction {
        self.bindings
            .get(&(context, gesture))
            .copied()
            .unwrap_or_else(|| gesture.default_action())
    }

    /// Assign one gesture. Direct single-click actions disable double-click for that context;
    /// a non-disabled double-click cannot subsequently be installed until single-click is made
    /// non-direct again.
    pub fn set(
        &mut self,
        context: MouseContext,
        gesture: MouseGesture,
        action: MouseAction,
    ) -> Result<(), MouseBindingError> {
        if !allowed_actions(context, gesture).contains(&action) {
            return Err(MouseBindingError::ActionNotAllowed {
                context,
                gesture,
                action,
            });
        }

        if gesture == MouseGesture::RightDoubleClick
            && action != MouseAction::Disabled
            && self
                .action(context, MouseGesture::RightClick)
                .is_direct_single_action()
        {
            return Err(MouseBindingError::DirectRightClickConflict {
                context,
                right_click: self.action(context, MouseGesture::RightClick),
            });
        }

        self.bindings.insert((context, gesture), action);
        if gesture == MouseGesture::RightClick && action.is_direct_single_action() {
            self.bindings.insert(
                (context, MouseGesture::RightDoubleClick),
                MouseAction::Disabled,
            );
        }
        Ok(())
    }

    /// Restore one gesture to its built-in value, subject to the same safety invariant as
    /// [`Self::set`].
    pub fn reset_binding(
        &mut self,
        context: MouseContext,
        gesture: MouseGesture,
    ) -> Result<(), MouseBindingError> {
        self.set(context, gesture, gesture.default_action())
    }

    /// Restore every mouse binding to its built-in value.
    pub fn reset_all(&mut self) {
        *self = Self::default();
    }

    /// Only deviations from defaults, suitable for [`crate::config::Config::mouse_bindings`].
    pub fn to_overrides(&self) -> BTreeMap<String, String> {
        let mut overrides = BTreeMap::new();
        for context in MouseContext::ALL {
            for gesture in MouseGesture::ALL {
                let action = self.action(context, gesture);
                if action != gesture.default_action() {
                    overrides.insert(
                        format!("{}.{}", context.id(), gesture.id()),
                        action.id().to_owned(),
                    );
                }
            }
        }
        overrides
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    #[test]
    fn defaults_cover_every_context_and_gesture() {
        let map = MouseMap::default();
        for context in MouseContext::ALL {
            assert_eq!(
                map.action(context, MouseGesture::RightClick),
                MouseAction::ContextMenu
            );
            assert_eq!(
                map.action(context, MouseGesture::RightDoubleClick),
                MouseAction::Activate
            );
        }
        assert!(map.to_overrides().is_empty());
        assert!(
            !allowed_actions(MouseContext::Queue, MouseGesture::RightClick)
                .contains(&MouseAction::Enqueue)
        );
        assert!(
            !allowed_actions(MouseContext::Search, MouseGesture::RightDoubleClick)
                .contains(&MouseAction::ContextMenu)
        );
    }

    #[test]
    fn override_round_trip_stores_only_deviations() {
        let mut map = MouseMap::default();
        map.set(
            MouseContext::Search,
            MouseGesture::RightDoubleClick,
            MouseAction::Enqueue,
        )
        .unwrap();
        map.set(
            MouseContext::Library,
            MouseGesture::RightClick,
            MouseAction::Activate,
        )
        .unwrap();

        let overrides = map.to_overrides();
        assert_eq!(overrides.len(), 3);
        assert_eq!(
            overrides
                .get("search.right_double_click")
                .map(String::as_str),
            Some("enqueue")
        );
        assert_eq!(
            overrides.get("library.right_click").map(String::as_str),
            Some("activate")
        );
        assert_eq!(
            overrides
                .get("library.right_double_click")
                .map(String::as_str),
            Some("disabled")
        );
        assert_eq!(MouseMap::from_overrides(&overrides), map);
    }

    #[test]
    fn malformed_unknown_and_disallowed_overrides_are_ignored() {
        let overrides = BTreeMap::from([
            ("broken".to_owned(), "activate".to_owned()),
            ("unknown.right_click".to_owned(), "disabled".to_owned()),
            ("search.unknown".to_owned(), "disabled".to_owned()),
            ("search.right_click".to_owned(), "unknown".to_owned()),
            ("queue.right_double_click".to_owned(), "enqueue".to_owned()),
            (
                "local.right_double_click".to_owned(),
                "context_menu".to_owned(),
            ),
        ]);
        assert_eq!(MouseMap::from_overrides(&overrides), MouseMap::default());
    }

    #[test]
    fn direct_right_click_disables_and_blocks_double_click() {
        let mut map = MouseMap::default();
        map.set(
            MouseContext::Library,
            MouseGesture::RightClick,
            MouseAction::Enqueue,
        )
        .unwrap();
        assert_eq!(
            map.action(MouseContext::Library, MouseGesture::RightDoubleClick),
            MouseAction::Disabled
        );

        assert_eq!(
            map.set(
                MouseContext::Library,
                MouseGesture::RightDoubleClick,
                MouseAction::Activate,
            ),
            Err(MouseBindingError::DirectRightClickConflict {
                context: MouseContext::Library,
                right_click: MouseAction::Enqueue,
            })
        );
        map.set(
            MouseContext::Library,
            MouseGesture::RightClick,
            MouseAction::ContextMenu,
        )
        .unwrap();
        map.reset_binding(MouseContext::Library, MouseGesture::RightDoubleClick)
            .unwrap();
        assert_eq!(
            map.action(MouseContext::Library, MouseGesture::RightDoubleClick),
            MouseAction::Activate
        );

        assert!(matches!(
            map.set(
                MouseContext::Queue,
                MouseGesture::RightClick,
                MouseAction::Enqueue,
            ),
            Err(MouseBindingError::ActionNotAllowed { .. })
        ));
    }

    #[test]
    fn reset_all_restores_defaults() {
        let mut map = MouseMap::default();
        map.set(
            MouseContext::Local,
            MouseGesture::RightDoubleClick,
            MouseAction::Disabled,
        )
        .unwrap();
        map.reset_all();
        assert_eq!(map, MouseMap::default());
    }

    #[test]
    fn config_defaults_and_json_round_trip_preserve_overrides() {
        let old: Config = serde_json::from_str("{}").unwrap();
        assert!(old.mouse_bindings.is_empty());
        assert_eq!(MouseMap::from_config(&old), MouseMap::default());

        let mut map = MouseMap::default();
        map.set(
            MouseContext::Search,
            MouseGesture::RightDoubleClick,
            MouseAction::Disabled,
        )
        .unwrap();
        let config = Config {
            mouse_bindings: map.to_overrides(),
            ..Config::default()
        };
        let json = serde_json::to_string(&config).unwrap();
        let restored: Config = serde_json::from_str(&json).unwrap();
        assert_eq!(MouseMap::from_config(&restored), map);
    }
}
