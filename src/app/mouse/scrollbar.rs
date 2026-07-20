use super::*;

impl App {
    pub(in crate::app) fn on_any_scrollbar_press(
        &mut self,
        target: &MouseTarget,
        rect: Rect,
        row: u16,
    ) -> Option<Vec<Cmd>> {
        match target {
            MouseTarget::Scrollbar(surface) => Some(self.on_scrollbar_press(*surface, rect, row)),
            MouseTarget::LocalFindScrollbar { stamp } => {
                Some(self.on_local_find_scrollbar_press(stamp.clone(), rect, row))
            }
            _ => None,
        }
    }

    pub(in crate::app) fn on_scrollbar_press(
        &mut self,
        surface: ScrollSurface,
        rect: Rect,
        row: u16,
    ) -> Vec<Cmd> {
        self.on_scrollbar_press_stamped(surface, rect, row, None)
    }

    pub(super) fn on_local_find_scrollbar_press(
        &mut self,
        stamp: LocalFindPointerStamp,
        rect: Rect,
        row: u16,
    ) -> Vec<Cmd> {
        if !self.local_find_pointer_stamp_is_live(&stamp) {
            return Vec::new();
        }
        self.on_scrollbar_press_stamped(ScrollSurface::LocalFind, rect, row, Some(stamp))
    }

    fn on_scrollbar_press_stamped(
        &mut self,
        surface: ScrollSurface,
        rect: Rect,
        row: u16,
        local_find_stamp: Option<LocalFindPointerStamp>,
    ) -> Vec<Cmd> {
        // Local Find must always use its generation-stamped target; fail closed if a future
        // caller accidentally routes it through the generic scrollbar path.
        if surface == ScrollSurface::LocalFind && local_find_stamp.is_none() {
            return Vec::new();
        }
        let Some((content_len, viewport, position)) = self.scrollbar_snapshot(surface) else {
            return Vec::new();
        };
        let track_row = row
            .saturating_sub(rect.y)
            .min(rect.height.saturating_sub(1));
        let Some(thumb) =
            crate::ui::scroll::scrollbar_thumb(content_len, viewport, rect.height, position)
        else {
            return Vec::new();
        };
        let thumb_end = thumb.start.saturating_add(thumb.len);
        let grab = if track_row >= thumb.start && track_row < thumb_end {
            track_row - thumb.start
        } else {
            thumb.len / 2
        };
        let drag = ScrollbarDrag {
            surface,
            rect,
            content_len,
            viewport,
            grab,
            local_find_stamp,
        };
        self.interaction.drag_selection = None;
        self.interaction.ai_transcript_drag = None;
        self.interaction.drag_scrollbar = Some(drag.clone());
        self.drag_scrollbar_to(&drag, row);
        Vec::new()
    }

    pub(super) fn drag_scrollbar_to(&mut self, drag: &ScrollbarDrag, row: u16) {
        if drag.surface == ScrollSurface::LocalFind
            && !drag
                .local_find_stamp
                .as_ref()
                .is_some_and(|stamp| self.local_find_pointer_stamp_is_live(stamp))
        {
            self.interaction.drag_scrollbar = None;
            return;
        }
        if drag.rect.height == 0 {
            return;
        }
        let track_row = row
            .saturating_sub(drag.rect.y)
            .min(drag.rect.height.saturating_sub(1));
        let offset = crate::ui::scroll::offset_from_scrollbar_row(
            track_row,
            drag.grab,
            drag.content_len,
            drag.viewport,
            drag.rect.height,
        );
        if let Some(state) = self.scroll_state(drag.surface) {
            state.set_offset(offset, drag.content_len);
            self.dirty = true;
        }
    }

    fn scrollbar_snapshot(&self, surface: ScrollSurface) -> Option<(usize, usize, usize)> {
        let state = self.scroll_state(surface)?;
        let content_len = self.scroll_content_len(surface)?;
        let viewport = state.viewport();
        if content_len <= viewport || viewport == 0 {
            return None;
        }
        Some((content_len, viewport, state.offset()))
    }

    fn scroll_state(&self, surface: ScrollSurface) -> Option<&crate::ui::scroll::ScrollState> {
        Some(match surface {
            ScrollSurface::Library => &self.bridges.library_scroll,
            ScrollSurface::Search => &self.bridges.search_scroll,
            ScrollSurface::LocalFind => &self.bridges.local_find_scroll,
            ScrollSurface::SearchFilter => &self.search_filter.scroll,
            ScrollSurface::ArtistSongs => {
                return self.search.artist.as_ref().map(|st| &st.songs_scroll);
            }
            ScrollSurface::ArtistAlbums => {
                return self.search.artist.as_ref().map(|st| &st.albums_scroll);
            }
            ScrollSurface::AiTranscript => &self.bridges.ai_transcript_scroll,
            ScrollSurface::AiSuggestions => &self.bridges.ai_scroll,
            ScrollSurface::Settings => &self.bridges.settings_scroll,
            ScrollSurface::Queue => &self.queue_popup.scroll,
            // Marquee-only surfaces with no scrollbar.
            ScrollSurface::NowPlaying | ScrollSurface::PlayerTitle => return None,
        })
    }

    fn scroll_content_len(&self, surface: ScrollSurface) -> Option<usize> {
        Some(match surface {
            ScrollSurface::Library => {
                if self.local_dedicated_mode {
                    self.local_rows_len()
                } else {
                    self.library_len()
                }
            }
            ScrollSurface::Search => self.search.results.len(),
            ScrollSurface::LocalFind => self.local_find_rows_len(),
            ScrollSurface::SearchFilter => self.search_filter.matches.len(),
            ScrollSurface::ArtistSongs => {
                return self.search.artist.as_ref().map(|st| st.page.songs.len());
            }
            ScrollSurface::ArtistAlbums => {
                return self.search.artist.as_ref().map(|st| st.page.albums.len());
            }
            ScrollSurface::AiTranscript => self.bridges.ai_transcript_copy_lines.borrow().len(),
            ScrollSurface::AiSuggestions => self.ai.suggestions.len(),
            ScrollSurface::Settings => self.settings_field_display_len()?,
            ScrollSurface::Queue => self.queue.len(),
            // Marquee-only surfaces with no scrollbar.
            ScrollSurface::NowPlaying | ScrollSurface::PlayerTitle => return None,
        })
    }

    fn settings_field_display_len(&self) -> Option<usize> {
        let st = self.settings.as_deref()?;
        if st.tab == SettingsTab::Keys {
            return None;
        }
        let fields = st.fields();
        // `st.sections()` (not `st.tab.sections()`) so the scroll length matches the
        // visibility-filtered field list in every mode.
        let sections = st.sections();
        Some(if sections.is_empty() {
            fields.len()
        } else {
            fields
                .len()
                .saturating_add(sections.len())
                .saturating_add(sections.len().saturating_sub(1))
        })
    }
}
