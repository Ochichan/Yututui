use std::collections::{BTreeMap, HashMap};

use crossterm::event::{KeyCode, KeyModifiers};

use super::compat;
use super::defaults::default_chord;
use super::metadata::all_contexts;
use super::{
    Action, Chord, KeyContext, chord_to_config, default_bindings, format_chord_for_display,
    parse_chord,
};

/// Why a rebind was rejected: `chord` is already bound to `existing` in context `ctx`
/// (the screen where it would have fired). Surfaced to the user as a warning popup so a
/// conflicting remap is reported loudly rather than silently dropped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Conflict {
    pub ctx: KeyContext,
    pub existing: Action,
    pub chord: Chord,
}

/// The resolved keybindings: chord → action (for dispatch) and action → chord (for
/// rendering hints), both keyed by context.
#[derive(Debug, Clone)]
pub struct KeyMap {
    bindings: HashMap<(KeyContext, Chord), Action>,
    labels: HashMap<(KeyContext, Action), Chord>,
}

impl Default for KeyMap {
    fn default() -> Self {
        Self::from_overrides(&BTreeMap::new())
    }
}

impl KeyMap {
    /// Build from a context/action → chord table, deriving the reverse lookup.
    fn from_labels(labels: HashMap<(KeyContext, Action), Chord>) -> Self {
        let mut bindings = HashMap::with_capacity(labels.len());
        for (&(ctx, action), &chord) in &labels {
            bindings.insert((ctx, chord), action);
        }
        KeyMap { bindings, labels }
    }

    /// Build from persisted overrides layered over the built-in defaults.
    pub fn from_overrides(overrides: &BTreeMap<String, String>) -> Self {
        let mut labels: HashMap<(KeyContext, Action), Chord> = default_bindings()
            .into_iter()
            .map(|(c, a, ch)| ((c, a), ch))
            .collect();
        for (key, val) in overrides {
            let Some((ctx_id, action_id)) = key.split_once('.') else {
                tracing::warn!(key, "ignoring malformed keybinding override");
                continue;
            };
            let Some(mut ctx) = KeyContext::from_id(ctx_id) else {
                tracing::warn!(key, value = val, "ignoring unknown keybinding override");
                continue;
            };
            let Some(action) = Action::from_id(action_id) else {
                if !(ctx_id == "settings" && action_id == "settings_save") {
                    tracing::warn!(key, value = val, "ignoring unknown keybinding override");
                }
                continue;
            };
            if val.is_empty() {
                // Explicitly unbound (the GUI's per-row unbind): drop the default.
                labels.remove(&(ctx, action));
                continue;
            }
            let Some(chord) = parse_chord(val) else {
                tracing::warn!(key, value = val, "ignoring unknown keybinding override");
                continue;
            };
            if ctx == KeyContext::Global && action == Action::ToggleRadioMode {
                ctx = KeyContext::Player;
            }
            labels.insert((ctx, action), chord);
        }
        compat::preserve_legacy_lyrics_delay_overrides(overrides, &mut labels);
        compat::preserve_legacy_shuffle_override(overrides, &mut labels);
        compat::preserve_legacy_text_edit_overrides(overrides, &mut labels);
        // Preserve the old Search-results shortcut as an unlisted compatibility binding:
        // the Player search key also focuses the query box from results. The new advertised
        // bidirectional binding is SearchInput/SearchResults FocusPrev (Shift+Tab).
        if !overrides.contains_key("search_results.focus_input")
            && let Some(&chord) = labels.get(&(KeyContext::Player, Action::OpenSearch))
        {
            let candidate = Self::from_labels(labels.clone());
            if let Some(conflict) =
                candidate.conflict(KeyContext::SearchResults, Action::FocusInput, chord)
            {
                tracing::warn!(
                    chord = %chord_to_config(chord),
                    conflict_ctx = ?conflict.ctx,
                    conflict_action = ?conflict.existing,
                    "not mirroring player.open_search to search_results.focus_input"
                );
            } else {
                labels.insert((KeyContext::SearchResults, Action::FocusInput), chord);
            }
        }
        Self::from_labels(labels)
    }

    pub fn from_config(cfg: &crate::config::Config) -> Self {
        Self::from_overrides(&cfg.keybindings)
    }

    /// Resolve the action bound to `chord` in `ctx`, falling back to the shared `Common`
    /// navigation bindings. Used by the per-screen key handlers.
    pub fn action(&self, ctx: KeyContext, chord: Chord) -> Option<Action> {
        self.bindings
            .get(&(ctx, chord))
            .or_else(|| self.bindings.get(&(KeyContext::Common, chord)))
            .copied()
    }

    /// Resolve only bindings declared directly on `ctx`, without the shared `Common`
    /// fallback. Text/list hybrids use this when a context-specific key intentionally
    /// shadows a common navigation key.
    pub fn context_action(&self, ctx: KeyContext, chord: Chord) -> Option<Action> {
        self.bindings.get(&(ctx, chord)).copied()
    }

    /// Resolve a `Global` action (help, streaming), independent of the active screen.
    pub fn global_action(&self, chord: Chord) -> Option<Action> {
        self.bindings.get(&(KeyContext::Global, chord)).copied()
    }

    /// Resolve the shared text-editing command before a focused editor considers typed input.
    /// Text fields capture their keyboard while active, so this deliberately bypasses a
    /// screen-specific binding that would otherwise shadow the Common editing action.
    pub fn text_edit_action(&self, chord: Chord) -> Option<Action> {
        self.bindings
            .get(&(KeyContext::Common, chord))
            .copied()
            .filter(|action| {
                matches!(
                    action,
                    Action::DeleteChar
                        | Action::DeleteWord
                        | Action::MoveCursorLeft
                        | Action::MoveCursorRight
                        | Action::MoveCursorWordLeft
                        | Action::MoveCursorWordRight
                )
            })
    }

    /// Whether an ambiguous legacy `^H` report should stand in for Ctrl+Backspace.
    ///
    /// Plain terminals can encode both physical keys as the same byte. Keep that byte reserved
    /// for safe word deletion only while DeleteWord still has its built-in Ctrl+Backspace chord.
    /// Remapping/unbinding DeleteWord releases `^H`; remapping Home alone deliberately does not.
    /// An explicit Common/Global `^H` claim also wins over the compatibility fallback.
    pub fn legacy_ctrl_backspace_fallback_active(&self) -> bool {
        let ctrl_backspace = Chord::new(KeyCode::Backspace, KeyModifiers::CONTROL);
        let ctrl_h = Chord::new(KeyCode::Char('h'), KeyModifiers::CONTROL);

        self.chord(KeyContext::Common, Action::DeleteWord) == Some(ctrl_backspace)
            && self.context_action(KeyContext::Common, ctrl_backspace) == Some(Action::DeleteWord)
            && self.context_action(KeyContext::Common, ctrl_h).is_none()
            && matches!(self.global_action(ctrl_h), None | Some(Action::Home))
    }

    /// The chord bound to `action` in `ctx`, formatted for the current display mode.
    pub fn label_for_display(&self, ctx: KeyContext, action: Action, retro: bool) -> String {
        let chord = self
            .labels
            .get(&(ctx, action))
            .or_else(|| self.labels.get(&(KeyContext::Common, action)))
            .or_else(|| self.labels.get(&(KeyContext::Global, action)))
            .copied();
        chord.map_or_else(
            || "?".to_owned(),
            |chord| format_chord_for_display(chord, retro),
        )
    }

    /// The chord currently bound to `(ctx, action)`, if any (for the editor).
    pub fn chord(&self, ctx: KeyContext, action: Action) -> Option<Chord> {
        self.labels.get(&(ctx, action)).copied()
    }

    /// If `chord` is already used by a *different* action that would win in the same
    /// routing scope, return the [`Conflict`] describing it. `Global` bindings are special:
    /// because they are consulted before every screen handler, they may not overlap any
    /// other context. Local contexts may shadow `Common` navigation, matching dispatch.
    fn conflict(&self, ctx: KeyContext, action: Action, chord: Chord) -> Option<Conflict> {
        if ctx == KeyContext::Global {
            return self.conflict_in_contexts(all_contexts(), action, chord);
        }

        self.conflict_in_context(ctx, action, chord)
            .or_else(|| self.conflict_in_context(KeyContext::Global, action, chord))
    }

    fn conflict_in_context(
        &self,
        ctx: KeyContext,
        action: Action,
        chord: Chord,
    ) -> Option<Conflict> {
        let existing = self.bindings.get(&(ctx, chord)).copied()?;
        let animation_shadow = chord == Chord::new(KeyCode::Char('A'), KeyModifiers::empty())
            && match ctx {
                KeyContext::Global => {
                    (existing, action) == (Action::ToggleAnimations, Action::AcceptAllImportReview)
                }
                KeyContext::LocalDeck => {
                    (existing, action) == (Action::AcceptAllImportReview, Action::ToggleAnimations)
                }
                _ => false,
            };
        if existing == action || animation_shadow {
            return None;
        }
        Some(Conflict {
            ctx,
            existing,
            chord,
        })
    }

    fn conflict_in_contexts(
        &self,
        contexts: impl IntoIterator<Item = KeyContext>,
        action: Action,
        chord: Chord,
    ) -> Option<Conflict> {
        contexts
            .into_iter()
            .find_map(|ctx| self.conflict_in_context(ctx, action, chord))
    }

    /// Rebind `(ctx, action)` to `chord`. Rejects (returns the [`Conflict`]) if the chord
    /// is already in use; otherwise drops the action's old chord and installs the new.
    pub fn rebind(
        &mut self,
        ctx: KeyContext,
        action: Action,
        chord: Chord,
    ) -> Result<(), Conflict> {
        for (target_ctx, target_action) in
            std::iter::once((ctx, action)).chain(linked_rebinds(ctx, action).iter().copied())
        {
            if let Some(conflict) = self.conflict(target_ctx, target_action, chord) {
                return Err(conflict);
            }
        }
        for (target_ctx, target_action) in
            std::iter::once((ctx, action)).chain(linked_rebinds(ctx, action).iter().copied())
        {
            if let Some(old) = self.labels.get(&(target_ctx, target_action)).copied() {
                self.bindings.remove(&(target_ctx, old));
            }
            self.bindings.insert((target_ctx, chord), target_action);
            self.labels.insert((target_ctx, target_action), chord);
        }
        Ok(())
    }

    /// Restore `(ctx, action)` to its built-in default chord. Returns the [`Conflict`] if
    /// the default chord is currently taken by something else.
    pub fn reset(&mut self, ctx: KeyContext, action: Action) -> Result<(), Conflict> {
        match default_chord(ctx, action) {
            Some(def) => self.rebind(ctx, action, def),
            None => Ok(()),
        }
    }

    /// Only the bindings that differ from the defaults, keyed `"<context>.<action>"`, for
    /// compact, forward-compatible persistence.
    pub fn to_overrides(&self) -> BTreeMap<String, String> {
        let mut out = BTreeMap::new();
        for (ctx, action, def) in default_bindings() {
            match self.labels.get(&(ctx, action)).copied() {
                // Explicitly unbound persists as "" (see `from_overrides`).
                None => {
                    out.insert(format!("{}.{}", ctx.id(), action.id()), String::new());
                }
                Some(cur) if cur != def => {
                    out.insert(
                        format!("{}.{}", ctx.id(), action.id()),
                        chord_to_config(cur),
                    );
                }
                Some(_) => {}
            }
        }
        out
    }

    /// Remove `(ctx, action)`'s binding entirely (the GUI's per-row unbind). The action
    /// simply stops dispatching until rebound or reset.
    pub fn unbind(&mut self, ctx: KeyContext, action: Action) {
        if let Some(old) = self.labels.remove(&(ctx, action)) {
            self.bindings.remove(&(ctx, old));
        }
    }

    /// The full effective wire bindings for the settings model
    /// (`"<ctx>.<action>"` → config chord string; `""` = unbound).
    pub fn wire_bindings(&self) -> BTreeMap<String, String> {
        let mut out = BTreeMap::new();
        for (ctx, action, _) in default_bindings() {
            let chord = self
                .labels
                .get(&(ctx, action))
                .map(|chord| chord_to_config(*chord))
                .unwrap_or_default();
            out.insert(format!("{}.{}", ctx.id(), action.id()), chord);
        }
        out
    }
}

fn linked_rebinds(ctx: KeyContext, action: Action) -> &'static [(KeyContext, Action)] {
    match (ctx, action) {
        (KeyContext::Player, Action::OpenSearch) => {
            &[(KeyContext::SearchResults, Action::FocusInput)]
        }
        (KeyContext::SearchInput, Action::FocusPrev) => {
            &[(KeyContext::SearchResults, Action::FocusPrev)]
        }
        (KeyContext::SearchResults, Action::FocusPrev) => {
            &[(KeyContext::SearchInput, Action::FocusPrev)]
        }
        (KeyContext::SearchInput, Action::ToggleSearchSourceMenu) => {
            &[(KeyContext::SearchResults, Action::ToggleSearchSourceMenu)]
        }
        (KeyContext::SearchResults, Action::ToggleSearchSourceMenu) => {
            &[(KeyContext::SearchInput, Action::ToggleSearchSourceMenu)]
        }
        (KeyContext::SearchInput, Action::ToggleSearchKind) => {
            &[(KeyContext::SearchResults, Action::ToggleSearchKind)]
        }
        (KeyContext::SearchResults, Action::ToggleSearchKind) => {
            &[(KeyContext::SearchInput, Action::ToggleSearchKind)]
        }
        _ => &[],
    }
}
