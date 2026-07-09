//! Spotify import settings and playlist-picker reducer helpers.

use super::*;

impl App {
    pub(in crate::app) fn settings_open_spotify_import_mode_dropdown(&mut self) {
        let Some(st) = self.settings.as_mut() else {
            return;
        };
        if st.current_field() != Some(Field::SpotifyImportMode) {
            return;
        }
        st.spotify_import_mode_dropdown = Some(st.draft.spotify_import_mode.index());
        self.status.text = t!(
            "Choose how Spotify imports write Library playlists",
            "Spotify 가져오기가 Library 플레이리스트를 쓰는 방식을 선택하세요"
        )
        .to_owned();
        self.status.kind = StatusKind::Info;
        self.dirty = true;
    }

    pub(in crate::app) fn settings_select_spotify_import_mode(
        &mut self,
        mode: crate::config::SpotifyImportMode,
    ) {
        let Some(st) = self.settings.as_mut() else {
            return;
        };
        st.draft.spotify_import_mode = mode;
        st.spotify_import_mode_dropdown = None;
        self.status.text = format!(
            "{}: {}",
            t!("Spotify import mode", "Spotify 가져오기 모드"),
            mode.label()
        );
        self.status.kind = StatusKind::Info;
        self.dirty = true;
    }

    pub(in crate::app) fn settings_close_spotify_import_mode_dropdown(&mut self) -> bool {
        let Some(st) = self.settings.as_mut() else {
            return false;
        };
        if st.spotify_import_mode_dropdown.take().is_some() {
            self.dirty = true;
            return true;
        }
        false
    }

    pub(in crate::app) fn settings_spotify_import_mode_dropdown_key(
        &mut self,
        k: KeyEvent,
    ) -> Vec<Cmd> {
        let action = self
            .keymap
            .action(KeyContext::Settings, k.into())
            .or_else(|| Self::settings_safety_action(k));
        let Some(mut selected) = self
            .settings
            .as_ref()
            .and_then(|st| st.spotify_import_mode_dropdown)
        else {
            return Vec::new();
        };
        let mut select_mode = None;
        match action {
            Some(Action::MoveUp) => {
                let len = crate::config::SpotifyImportMode::ALL.len();
                selected = (selected + len - 1) % len;
                if let Some(st) = self.settings.as_mut() {
                    st.spotify_import_mode_dropdown = Some(selected);
                }
                self.dirty = true;
            }
            Some(Action::MoveDown) => {
                let len = crate::config::SpotifyImportMode::ALL.len();
                selected = (selected + 1) % len;
                if let Some(st) = self.settings.as_mut() {
                    st.spotify_import_mode_dropdown = Some(selected);
                }
                self.dirty = true;
            }
            Some(Action::Confirm) => {
                select_mode = crate::config::SpotifyImportMode::ALL.get(selected).copied();
            }
            Some(Action::SettingsCancel | Action::Back) => {
                if let Some(st) = self.settings.as_mut() {
                    st.spotify_import_mode_dropdown = None;
                }
                self.dirty = true;
            }
            _ => {}
        }
        if let Some(mode) = select_mode {
            self.settings_select_spotify_import_mode(mode);
        }
        Vec::new()
    }

    /// Keys while the Spotify playlist picker overlay is open (up/down/Enter/Esc).
    pub(in crate::app) fn spotify_picker_key(&mut self, k: KeyEvent) -> Vec<Cmd> {
        let action = self
            .keymap
            .action(KeyContext::Settings, k.into())
            .or_else(|| Self::settings_safety_action(k));
        let Some(picker) = self.overlays.spotify_picker.as_mut() else {
            return Vec::new();
        };
        self.dirty = true;
        match action {
            Some(Action::MoveUp) => {
                picker.selected = picker.selected.saturating_sub(1);
                Vec::new()
            }
            Some(Action::MoveDown) => {
                picker.selected = (picker.selected + 1).min(picker.items.len().saturating_sub(1));
                Vec::new()
            }
            Some(Action::Confirm) => self.spotify_picker_confirm(),
            Some(Action::SettingsCancel | Action::Back) => {
                self.overlays.spotify_picker = None;
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    /// Start importing the picker's selected item, shared by keyboard and mouse confirm paths.
    pub(in crate::app) fn spotify_picker_confirm(&mut self) -> Vec<Cmd> {
        let Some(item) = self
            .overlays
            .spotify_picker
            .as_ref()
            .and_then(|p| p.items.get(p.selected).cloned())
        else {
            return Vec::new();
        };
        self.overlays.spotify_picker = None;
        self.transfer_running = true;
        self.dirty = true;
        let import_mode = self
            .settings
            .as_ref()
            .map(|st| st.draft.spotify_import_mode)
            .unwrap_or(self.config.spotify.import_mode);
        let (dry_run, auto_accept_ambiguous_min_score) = match import_mode {
            crate::config::SpotifyImportMode::FastPlaylist => (false, Some(0.75)),
            crate::config::SpotifyImportMode::StrictPlaylist => (false, None),
            crate::config::SpotifyImportMode::ReviewFirst => (true, None),
        };
        let spec = crate::transfer::JobSpec {
            source: item.source,
            dest: crate::transfer::TransferDest::LocalPlaylist { name: None },
            dry_run,
            min_score: 0.80,
            take_best: false,
            auto_accept_ambiguous_min_score,
            match_policy: match import_mode {
                crate::config::SpotifyImportMode::FastPlaylist => {
                    crate::transfer::MatchPolicy::Balanced
                }
                crate::config::SpotifyImportMode::StrictPlaylist
                | crate::config::SpotifyImportMode::ReviewFirst => {
                    crate::transfer::MatchPolicy::Strict
                }
            },
            allow_user_videos: false,
            rematch: false,
        };
        self.status.text = if crate::i18n::is_korean() {
            format!(
                "가져오는 중: {} · 모드: {}",
                item.label,
                import_mode.label()
            )
        } else {
            format!("Importing: {} · mode: {}", item.label, import_mode.label())
        };
        self.status.kind = StatusKind::Info;
        vec![Cmd::Transfer(
            crate::transfer::actor::TransferCmd::StartJob(Box::new(spec)),
        )]
    }
}
