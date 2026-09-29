use super::*;

impl App {
    /// Single-click select on the active screen's list. `index` is the logical item index
    /// the view published (song index, or a Settings row index).
    pub(in crate::app) fn on_list_row_click(&mut self, index: usize) -> Vec<Cmd> {
        match self.mode {
            Mode::Search if index < self.search.results.len() => {
                self.interaction.pending_double_click_selection = {
                    let selection = self.search_selection_indices();
                    (selection.len() > 1 && selection.contains(&index)).then_some(
                        PendingDoubleClickSelection {
                            surface: DragSurface::Search,
                            row: index,
                            indices: selection,
                        },
                    )
                };
                self.search.selected = index;
                self.search.anchor = index;
                // A plain click always has immediate single-row semantics. If it turns out
                // to be the first half of a double-click, the matching activation path above
                // restores the pre-click selection from the transient snapshot.
                self.search.picked.clear();
                self.search.focus = SearchFocus::Results;
                self.interaction.drag_selection = Some(DragSelection {
                    surface: DragSurface::Search,
                    anchor: index,
                });
                self.dirty = true;
            }
            Mode::Library if self.local_dedicated_mode => return self.local_row_click(index),
            Mode::Library if index < self.library_len() => {
                self.interaction.pending_double_click_selection = {
                    let selection = self.library_selection_indices();
                    (!self.playlists_root() && selection.len() > 1 && selection.contains(&index))
                        .then_some(PendingDoubleClickSelection {
                            surface: DragSurface::Library,
                            row: index,
                            indices: selection,
                        })
                };
                self.library_ui.selected = index;
                self.library_ui.anchor = index;
                self.library_ui.picked.clear();
                self.interaction.drag_selection = Some(DragSelection {
                    surface: DragSurface::Library,
                    anchor: index,
                });
                self.dirty = true;
            }
            Mode::Settings => {
                // A whole-row click focuses the row; on an actionable Button row it also
                // activates it (the Enter equivalent) so clicking anywhere on e.g. the Spotify
                // "connect in browser" / "import" row works, not just the small value glyph.
                // Text/toggle/slider rows stay focus-only here — their value hit-rects own edit.
                let is_button = self
                    .settings
                    .as_ref()
                    .and_then(|st| st.fields().get(index).copied())
                    .is_some_and(|f| matches!(f.kind(), crate::settings::FieldKind::Button));
                if is_button {
                    self.settings_focus_row(index);
                    return self.settings_activate();
                }
                if let Some(st) = self.settings.as_mut() {
                    st.row = index;
                    st.editing_text = false;
                    self.dirty = true;
                }
            }
            _ => {}
        }
        Vec::new()
    }

    /// Restore a selection hidden by the first press of this exact double-click. The snapshot
    /// is one-shot and surface/row scoped, so a double-click on any other list row activates
    /// only that clicked row.
    pub(super) fn restore_double_click_selection(&mut self, index: usize) {
        let surface = match self.mode {
            Mode::Search => Some(DragSurface::Search),
            Mode::Library if !self.local_dedicated_mode => Some(DragSurface::Library),
            _ => None,
        };
        let Some(snapshot) = self.interaction.pending_double_click_selection.take() else {
            return;
        };
        if surface != Some(snapshot.surface) || snapshot.row != index {
            return;
        }
        match snapshot.surface {
            DragSurface::Search => {
                self.search.selected = index;
                self.search.anchor = index;
                self.search.picked = snapshot.indices.into_iter().collect();
            }
            DragSurface::Library => {
                self.library_ui.selected = index;
                self.library_ui.anchor = index;
                self.library_ui.picked = snapshot.indices.into_iter().collect();
            }
            DragSurface::Queue => {}
        }
    }

    /// Double-click activate on the active screen's list: play the song now, keeping the queue
    /// (Search/Library) — the mouse equivalent of Enter. Settings rows have no "play", so a
    /// double-click just selects.
    pub(in crate::app) fn on_list_row_activate(&mut self, index: usize) -> Vec<Cmd> {
        match self.mode {
            // The shared activation path, so a double-clicked playlist row fetches its
            // tracks first (like Enter) instead of trying to play the row itself.
            Mode::Search if index < self.search.results.len() => {
                match self.multi_selected_search_songs() {
                    Some(songs) => self.play_now_many(songs),
                    None => self.activate_search_index(index),
                }
            }
            Mode::Library if self.local_dedicated_mode => self.local_row_activate(index),
            Mode::Library if index < self.library_len() => {
                self.library_ui.selected = index;
                // At the Playlists root a double-click opens the playlist (the row is a
                // playlist, not a song) — the mouse equivalent of Enter there too.
                if self.playlists_root() {
                    self.library_ui.anchor = index;
                    return self.open_selected_playlist();
                }
                self.play_now_many(self.selected_library_songs())
            }
            _ => self.on_list_row_click(index),
        }
    }
}
