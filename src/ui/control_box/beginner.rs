use crate::app::{App, Mode};
use crate::keymap::{Action, KeyContext, format_chord_for_display};
use crate::ui::buttons;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum StatusLabelTier {
    Compact,
    BeginnerNames,
    BeginnerKeys,
}

impl StatusLabelTier {
    pub(super) fn beginner(self) -> bool {
        self != Self::Compact
    }

    pub(super) fn keycaps(self) -> bool {
        self == Self::BeginnerKeys
    }
}

/// Expand a compact player control into its beginner-facing name and optional state.
/// Player-context keycaps are only honest on the Player screen: docked bars on other
/// screens still expose the mouse target, but those keys route through that screen's context.
pub(super) fn control_label(
    app: &App,
    labels: StatusLabelTier,
    context: KeyContext,
    action: Action,
    name: &str,
    state: Option<String>,
) -> String {
    let show_keycap =
        labels.keycaps() && (context != KeyContext::Player || app.mode == Mode::Player);
    let keycap = if show_keycap {
        app.keymap
            .chord(context, action)
            .map(|chord| format!("[{}] ", format_chord_for_display(chord, app.retro_mode())))
            .unwrap_or_default()
    } else {
        String::new()
    };
    match state {
        Some(state) => format!("{keycap}{name}: {state}"),
        None => format!("{keycap}{name}"),
    }
}

pub(super) fn pad_display_state(value: &str, states: &[&str]) -> String {
    let width = states
        .iter()
        .map(|state| buttons::text_width(state))
        .max()
        .unwrap_or_default();
    let mut value = value.to_owned();
    value.push_str(&" ".repeat(usize::from(
        width.saturating_sub(buttons::text_width(&value)),
    )));
    value
}

/// Beginner volume buttons include their live Player bindings when those bindings are
/// active. An unbound action, or a docked bar outside Player mode, keeps the plain +/- button.
pub(super) fn volume_buttons(app: &App) -> (String, String) {
    if app.mode != Mode::Player {
        return (" - ".to_owned(), " + ".to_owned());
    }
    let keycap = |action| {
        app.keymap
            .chord(KeyContext::Player, action)
            .map(|chord| format_chord_for_display(chord, app.retro_mode()))
    };
    let down = keycap(Action::VolDown)
        .map(|key| format!(" [{key}] - "))
        .unwrap_or_else(|| " - ".to_owned());
    let up = keycap(Action::VolUp)
        .map(|key| format!(" + [{key}] "))
        .unwrap_or_else(|| " + ".to_owned());
    (down, up)
}

#[cfg(test)]
mod tests {
    use super::super::{StatusLinePart, fitted_status_line_parts};
    use crate::app::{App, Mode, MouseTarget};
    use crate::keymap::{Action, KeyContext};

    fn text_for<'a>(parts: &'a [StatusLinePart], target: &MouseTarget) -> &'a str {
        parts
            .iter()
            .find(|(candidate, _)| candidate.as_ref() == Some(target))
            .map(|(_, text)| text.as_ref())
            .unwrap_or_else(|| panic!("missing {target:?}"))
    }

    #[test]
    fn keycaps_follow_rebinds_and_omit_unbound_actions() {
        let _guard = crate::i18n::lock_for_test();
        let mut app = App::new(100);
        app.config.beginner_mode = true;
        app.keymap
            .rebind(
                KeyContext::Player,
                Action::ToggleShuffle,
                crate::keymap::parse_chord("f8").unwrap(),
            )
            .unwrap();
        app.keymap.unbind(KeyContext::Player, Action::CycleRepeat);

        let (_, parts) = fitted_status_line_parts(&app, 200, false);
        assert!(text_for(&parts, &MouseTarget::Player(Action::ToggleShuffle)).starts_with("[F8] "));
        assert!(
            text_for(&parts, &MouseTarget::Player(Action::CycleRepeat)).starts_with("Repeat: ")
        );
    }

    #[test]
    fn docked_labels_omit_inactive_player_keycaps() {
        let _guard = crate::i18n::lock_for_test();
        let mut app = App::new(100);
        app.config.beginner_mode = true;
        app.mode = Mode::Settings;

        let (_, parts) = fitted_status_line_parts(&app, 200, false);
        for (target, name) in [
            (MouseTarget::Player(Action::ToggleShuffle), "Shuffle: "),
            (MouseTarget::Player(Action::CycleRepeat), "Repeat: "),
            (MouseTarget::EqMenu, "Equalizer: "),
        ] {
            let text = text_for(&parts, &target);
            assert!(text.starts_with(name), "got {text:?}");
            assert!(!text.contains('['), "inactive keycap leaked into {text:?}");
        }
    }

    #[test]
    fn streaming_label_combines_toggle_state_and_mode() {
        let _guard = crate::i18n::lock_for_test();
        let mut app = App::new(100);
        app.config.beginner_mode = true;
        app.autoplay_streaming = true;
        app.config.streaming.mode = crate::streaming::StreamingMode::Balanced;

        let (_, player_parts) = fitted_status_line_parts(&app, 240, false);
        assert_eq!(
            text_for(&player_parts, &MouseTarget::StreamingMenu).trim_end(),
            "[^r] Streaming autoplay: On · Balanced"
        );

        // The streaming toggle is global, so unlike Player actions its keycap remains honest
        // in a docked bar on another screen.
        app.mode = Mode::Settings;
        let (_, docked_parts) = fitted_status_line_parts(&app, 240, false);
        assert_eq!(
            text_for(&docked_parts, &MouseTarget::StreamingMenu).trim_end(),
            "[^r] Streaming autoplay: On · Balanced"
        );

        app.local_dedicated_mode = true;
        let (_, local_parts) = fitted_status_line_parts(&app, 240, false);
        assert_eq!(
            text_for(&local_parts, &MouseTarget::StreamingMenu).trim_end(),
            "[^r] Streaming autoplay: Off (saved On)"
        );
    }
}
