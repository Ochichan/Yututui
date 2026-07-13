//! GUI keymap / theme-override / romanization-cache commands (C6, daemon-only).
//!
//! The daemon holds no live dispatcher: keymap edits go config → `KeyMap` round-trip so
//! conflict detection and persistence share the TUI's exact rules, and the settings
//! topic re-pushes the full effective map afterwards (the publisher's byte-compare).

use crate::keymap::{Action, KeyContext, KeyMap, parse_chord};
use crate::remote::proto::{GuiSettingChange, KeymapConflictModel, RemoteResponse, ResponseData};

use super::{DaemonEngine, EngineEffect};

impl DaemonEngine {
    /// `keymap_bind`: conflict-checked like the TUI's editor. On conflict the bind is
    /// NOT applied — the reply carries the shadowed binding and the authoritative
    /// settings push reconciles the GUI's optimistic chord back.
    pub(super) fn gui_keymap_bind(
        &mut self,
        context: &str,
        action: &str,
        chord: &str,
    ) -> RemoteResponse {
        let (Some(ctx), Some(action)) = (KeyContext::from_id(context), Action::from_id(action))
        else {
            return RemoteResponse::err("bad_request");
        };
        let Some(chord) = parse_chord(chord) else {
            return RemoteResponse::err("bad_request");
        };
        let mut keymap = KeyMap::from_config(&self.config);
        match keymap.rebind(ctx, action, chord) {
            Ok(()) => {
                self.config.keybindings = keymap.to_overrides();
                self.save_config("daemon keymap bind");
                RemoteResponse::ok("binding updated".to_owned())
            }
            Err(conflict) => {
                let mut response = RemoteResponse::ok("binding shadowed".to_owned());
                response.data = Some(ResponseData::KeymapConflict {
                    conflict: KeymapConflictModel {
                        shadows: format!(
                            "{} — {}",
                            conflict.ctx.id(),
                            conflict.existing.human_label_for(conflict.ctx)
                        ),
                    },
                });
                response
            }
        }
    }

    pub(super) fn gui_keymap_unbind(&mut self, context: &str, action: &str) -> RemoteResponse {
        let (Some(ctx), Some(action)) = (KeyContext::from_id(context), Action::from_id(action))
        else {
            return RemoteResponse::err("bad_request");
        };
        let mut keymap = KeyMap::from_config(&self.config);
        keymap.unbind(ctx, action);
        self.config.keybindings = keymap.to_overrides();
        self.save_config("daemon keymap unbind");
        RemoteResponse::ok("binding removed".to_owned())
    }

    pub(super) fn gui_keymap_reset_all(&mut self) -> RemoteResponse {
        self.config.keybindings.clear();
        self.save_config("daemon keymap reset");
        RemoteResponse::ok("keymap reset".to_owned())
    }

    /// `theme_set_override` rides the existing `apply { theme.<role> = hex }` lane so
    /// validation and any live hooks stay single-sourced.
    pub(super) fn gui_theme_set_override(
        &mut self,
        role: String,
        hex: String,
    ) -> (RemoteResponse, Vec<EngineEffect>) {
        self.apply_gui_setting(GuiSettingChange {
            group: "theme".to_owned(),
            field: role,
            value: serde_json::Value::String(hex),
        })
    }

    pub(super) fn gui_theme_clear_override(&mut self, role: &str) -> RemoteResponse {
        let Some(role) = crate::theme::ThemeRole::ALL
            .into_iter()
            .find(|candidate| candidate.id() == role)
        else {
            return RemoteResponse::err("unknown_setting");
        };
        self.config.theme.overrides.remove(role.id());
        self.save_config("daemon theme clear override");
        RemoteResponse::ok("theme override cleared".to_owned())
    }

    /// `clear_romanization_cache`: the persisted cache is shared with the TUI; the
    /// count rides the reply's data lane (`{ cleared }`).
    pub(super) fn gui_clear_romanization_cache(&mut self) -> RemoteResponse {
        let mut cache = crate::romanize::RomanizeCache::load();
        let cleared = cache.len() as u64;
        cache.clear();
        if let Err(error) = cache.save() {
            tracing::warn!(%error, "romanization cache clear failed to persist");
            return RemoteResponse::err("durability_unconfirmed");
        }
        let mut response = RemoteResponse::ok("romanization cache cleared".to_owned());
        response.data = Some(ResponseData::Cleared { cleared });
        response
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn keymap_bind_applies_and_conflicts_report_without_applying() {
        let mut engine = super::super::tests::engine_with_queue(&[]);
        engine.remote_persistence_command_active = true;
        engine.remote_persistence_read_only = true;

        // A clean rebind persists an override.
        let ok = engine.gui_keymap_bind("player", "toggle_pause", "ctrl+p");
        assert!(ok.ok, "{ok:?}");
        assert!(ok.data.is_none());
        assert!(!engine.config.keybindings.is_empty());

        // Binding onto an existing chord reports the shadowed action, applies nothing.
        let before = engine.config.keybindings.clone();
        let conflicted = engine.gui_keymap_bind("player", "next_track", "ctrl+p");
        assert!(conflicted.ok);
        assert!(matches!(
            conflicted.data,
            Some(crate::remote::proto::ResponseData::KeymapConflict { .. })
        ));
        assert_eq!(engine.config.keybindings, before);

        // Unknown ids / chords are rejected.
        assert!(!engine.gui_keymap_bind("nope", "toggle_pause", "x").ok);
        assert!(!engine.gui_keymap_bind("player", "nope", "x").ok);
        assert!(!engine.gui_keymap_bind("player", "toggle_pause", "").ok);
    }

    #[test]
    fn keymap_unbind_persists_and_reset_all_restores_defaults() {
        let mut engine = super::super::tests::engine_with_queue(&[]);
        engine.remote_persistence_command_active = true;
        engine.remote_persistence_read_only = true;

        assert!(engine.gui_keymap_unbind("player", "toggle_pause").ok);
        assert_eq!(
            engine
                .config
                .keybindings
                .get("player.toggle_pause")
                .map(String::as_str),
            Some(""),
            "unbound persists as the empty override"
        );
        // The wire map reflects the unbound row.
        let keymap = crate::keymap::KeyMap::from_config(&engine.config);
        assert_eq!(
            keymap
                .wire_bindings()
                .get("player.toggle_pause")
                .map(String::as_str),
            Some("")
        );

        assert!(engine.gui_keymap_reset_all().ok);
        assert!(engine.config.keybindings.is_empty());
    }

    #[test]
    fn theme_overrides_set_and_clear_through_the_config() {
        let mut engine = super::super::tests::engine_with_queue(&[]);
        engine.remote_persistence_command_active = true;
        engine.remote_persistence_read_only = true;

        let (set, _) = engine.gui_theme_set_override("accent".to_owned(), "#aabbcc".to_owned());
        assert!(set.ok, "{set:?}");
        assert!(engine.config.theme.overrides.contains_key("accent"));
        assert!(engine.gui_theme_clear_override("accent").ok);
        assert!(!engine.config.theme.overrides.contains_key("accent"));
        assert!(!engine.gui_theme_clear_override("nonsense").ok);
    }
}
