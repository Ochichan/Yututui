//! Bounded, requester-scoped GUI search rows used by play/enqueue commands.

use std::collections::{HashMap, VecDeque};

use crate::api::{GuiSearchGroup, Song};
use crate::remote::MAX_SESSIONS;
use crate::remote::proto::REMOTE_MAX_TRACK_IDS;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(in crate::daemon) struct RequesterKey {
    session_id: u64,
    page_id: Option<String>,
}

impl RequesterKey {
    pub(in crate::daemon) fn new(session_id: u64, page_id: Option<String>) -> Self {
        Self {
            session_id,
            page_id,
        }
    }

    pub(in crate::daemon) fn session_id(&self) -> u64 {
        self.session_id
    }

    pub(in crate::daemon) fn page_id(&self) -> Option<&str> {
        self.page_id.as_deref()
    }
}

struct RequesterRows {
    key: RequesterKey,
    latest_ticket: u64,
    query: String,
    source: crate::search_source::SearchSource,
    active: bool,
    songs: HashMap<String, Song>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum GuiSearchAdmission {
    Start,
    DuplicateActive,
    StaleTicket,
    TicketConflict,
}

#[derive(Default)]
pub(super) struct GuiSearchIndex {
    /// Oldest requester first. Entries are inert values, never live socket handles. Closed
    /// sessions may remain until FIFO eviction, but the remote session cap bounds retention and
    /// the hub's non-wrapping session ids prevent a retained key from aliasing a reconnect.
    requesters: VecDeque<RequesterRows>,
}

impl GuiSearchIndex {
    pub(super) fn begin(
        &mut self,
        key: &RequesterKey,
        ticket: u64,
        query: &str,
        source: crate::search_source::SearchSource,
    ) -> GuiSearchAdmission {
        if let Some(position) = self.requesters.iter().position(|rows| rows.key == *key) {
            let mut rows = self
                .requesters
                .remove(position)
                .expect("the requester position was just found");
            let admission = if ticket < rows.latest_ticket {
                GuiSearchAdmission::StaleTicket
            } else if ticket == rows.latest_ticket && (rows.query != query || rows.source != source)
            {
                GuiSearchAdmission::TicketConflict
            } else if ticket == rows.latest_ticket && rows.active {
                GuiSearchAdmission::DuplicateActive
            } else {
                rows.latest_ticket = ticket;
                rows.query = query.to_owned();
                rows.source = source;
                rows.active = true;
                rows.songs.clear();
                GuiSearchAdmission::Start
            };
            if admission == GuiSearchAdmission::Start {
                self.requesters.push_back(rows);
            } else {
                self.requesters.insert(position, rows);
            }
            return admission;
        }

        // A replacement page owns the session's sole row. Removing every older page also keeps
        // the global value index aligned with the remote hub's session cap.
        self.requesters
            .retain(|rows| rows.key.session_id != key.session_id);
        while self.requesters.len() >= MAX_SESSIONS {
            self.requesters.pop_front();
        }
        self.requesters.push_back(RequesterRows {
            key: key.clone(),
            latest_ticket: ticket,
            query: query.to_owned(),
            source,
            active: true,
            songs: HashMap::new(),
        });
        GuiSearchAdmission::Start
    }

    pub(super) fn is_current(&self, key: &RequesterKey, ticket: u64) -> bool {
        self.requesters
            .iter()
            .rev()
            .find(|rows| rows.key == *key)
            .is_some_and(|rows| rows.active && rows.latest_ticket == ticket)
    }

    pub(super) fn complete(
        &mut self,
        key: &RequesterKey,
        ticket: u64,
        groups: &[GuiSearchGroup],
    ) -> bool {
        let songs = groups
            .iter()
            .flat_map(|group| group.songs.iter())
            .take(REMOTE_MAX_TRACK_IDS)
            .map(|song| (crate::api::gui_search_row_id(song), song.clone()))
            .collect();
        let Some(rows) = self
            .requesters
            .iter_mut()
            .rev()
            .find(|rows| rows.key == *key && rows.active && rows.latest_ticket == ticket)
        else {
            return false;
        };
        rows.songs = songs;
        rows.active = false;
        true
    }

    pub(super) fn resolve(&self, key: &RequesterKey, video_id: &str) -> Option<Song> {
        self.requesters
            .iter()
            .rev()
            .find(|rows| rows.key == *key)
            .and_then(|rows| rows.songs.get(video_id))
            .cloned()
    }

    #[cfg(test)]
    pub(super) fn len(&self) -> usize {
        self.requesters.len()
    }
}
