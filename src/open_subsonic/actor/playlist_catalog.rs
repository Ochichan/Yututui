//! Playlist access evidence and combined remote/recovery pagination.

use std::collections::BTreeSet;

use age::secrecy::ExposeSecret as _;
use sha2::{Digest as _, Sha256};

use super::super::bridge_store::{OpenSubsonicBridgeState, PlaylistLinkState};
use super::super::client::ServerError;
use super::super::model::{
    ServerLibraryDetail, ServerLibraryPage, ServerLibraryRow, ServerLibrarySection,
    ServerPlaylistAccess, ServerPlaylistLinkHealth, ServerPlaylistLinkSummary,
    ServerPlaylistSummary,
};
use super::super::private_store::ServerCredential;
use super::ServerLibraryRequest;

#[derive(Clone, PartialEq, Eq)]
struct RecoveryPrefixVersion {
    count: u32,
    revision: [u8; 32],
}

pub(super) struct PlaylistCatalogPagePlan {
    remote_offset: u32,
    remote_limit: u32,
    recovery_prefix: Option<RecoveryPrefixVersion>,
}

impl PlaylistCatalogPagePlan {
    pub(super) const fn remote_window(&self) -> (u32, u32) {
        (self.remote_offset, self.remote_limit)
    }
}

#[derive(Default)]
pub(super) struct PlaylistCatalogSession {
    recovery_prefix: Option<RecoveryPrefixVersion>,
    issued_offsets: BTreeSet<u32>,
}

impl PlaylistCatalogSession {
    pub(super) fn start_page(
        &mut self,
        request: ServerLibraryRequest,
        bridge_state: &OpenSubsonicBridgeState,
    ) -> Result<PlaylistCatalogPagePlan, ServerError> {
        let (remote_offset, remote_limit) = playlist_catalog_window(request, bridge_state);
        if request.section != ServerLibrarySection::Playlists || request.limit == 0 {
            return Ok(PlaylistCatalogPagePlan {
                remote_offset,
                remote_limit,
                recovery_prefix: None,
            });
        }

        let current = recovery_prefix_version(bridge_state);
        if request.offset == 0 {
            self.recovery_prefix = Some(current.clone());
            self.issued_offsets.clear();
            self.issued_offsets.insert(0);
        } else if self.recovery_prefix.as_ref() != Some(&current)
            || !self.issued_offsets.contains(&request.offset)
        {
            self.recovery_prefix = None;
            self.issued_offsets.clear();
            return Err(ServerError::TemporarilyUnavailable);
        }
        Ok(PlaylistCatalogPagePlan {
            remote_offset,
            remote_limit,
            recovery_prefix: Some(current),
        })
    }

    pub(super) fn finish_page(
        &mut self,
        page: &mut ServerLibraryPage,
        bridge_state: &OpenSubsonicBridgeState,
        request: ServerLibraryRequest,
        plan: &PlaylistCatalogPagePlan,
    ) -> Result<(), ServerError> {
        let Some(expected) = &plan.recovery_prefix else {
            return Ok(());
        };
        if recovery_prefix_version(bridge_state) != *expected {
            self.recovery_prefix = None;
            self.issued_offsets.clear();
            return Err(ServerError::TemporarilyUnavailable);
        }
        merge_missing_playlist_recovery_rows(page, bridge_state, request.offset, request.limit);
        if let Some(next_offset) = page.next_offset {
            self.issued_offsets.insert(next_offset);
        }
        Ok(())
    }
}

pub(super) fn finalize_page_playlist_access(
    page: &mut ServerLibraryPage,
    credential: &ServerCredential,
    bridge_state: &OpenSubsonicBridgeState,
) {
    for row in &mut page.rows {
        if let ServerLibraryRow::Playlist(summary) = row {
            finalize_playlist_access(summary, credential, bridge_state);
        }
    }
}

pub(super) fn finalize_detail_playlist_access(
    detail: &mut ServerLibraryDetail,
    credential: &ServerCredential,
    bridge_state: &OpenSubsonicBridgeState,
) {
    if let ServerLibraryDetail::PlaylistEntries(playlist) = detail {
        finalize_playlist_access(&mut playlist.summary, credential, bridge_state);
    }
}

fn finalize_playlist_access(
    summary: &mut ServerPlaylistSummary,
    credential: &ServerCredential,
    bridge_state: &OpenSubsonicBridgeState,
) {
    if let Some(link) = bridge_state
        .playlist_links()
        .values()
        .find(|link| link.server_playlist_id == summary.id)
    {
        summary.access = ServerPlaylistAccess::Linked;
        summary.link = Some(ServerPlaylistLinkSummary {
            local_playlist_id: link.local_playlist_id.clone(),
            health: match link.state {
                PlaylistLinkState::ServerMissing => ServerPlaylistLinkHealth::ServerMissing,
                PlaylistLinkState::AccessNeedsAttention => {
                    ServerPlaylistLinkHealth::NeedsAttention
                }
                PlaylistLinkState::Linked if link.content_needs_attention => {
                    ServerPlaylistLinkHealth::NeedsAttention
                }
                PlaylistLinkState::Linked
                    if !has_exact_playlist_write_access(summary, credential)
                        || bridge_state
                            .pending_playlist_projections()
                            .get(&link.local_playlist_id)
                            .is_some_and(|pending| {
                                pending.stage
                                    == crate::open_subsonic::bridge_store::PendingPlaylistProjectionStage::NeedsAttention
                            }) =>
                {
                    ServerPlaylistLinkHealth::NeedsAttention
                }
                PlaylistLinkState::Linked => ServerPlaylistLinkHealth::UpToDate,
            },
        });
        return;
    }
    summary.link = None;
    let credential_owner_is_exact = credential
        .username()
        .is_some_and(|username| summary.owner_evidence() == Some(username.expose_secret()));
    summary.access = if summary.readonly_evidence() == Some(false) && credential_owner_is_exact {
        ServerPlaylistAccess::Server
    } else {
        ServerPlaylistAccess::ReadOnly
    };
}

fn has_exact_playlist_write_access(
    summary: &ServerPlaylistSummary,
    credential: &ServerCredential,
) -> bool {
    summary.readonly_evidence() == Some(false)
        && credential
            .username()
            .is_some_and(|username| summary.owner_evidence() == Some(username.expose_secret()))
}

/// Translate a combined recovery/remote cursor into the remote catalog window.
pub(super) fn playlist_catalog_window(
    request: ServerLibraryRequest,
    bridge_state: &OpenSubsonicBridgeState,
) -> (u32, u32) {
    if request.section != ServerLibrarySection::Playlists || request.limit == 0 {
        return (request.offset, request.limit);
    }
    let missing = missing_playlist_link_count(bridge_state);
    let missing_on_page = missing.saturating_sub(request.offset).min(request.limit);
    let remote_slots = request.limit.saturating_sub(missing_on_page);
    (request.offset.saturating_sub(missing), remote_slots.max(1))
}

/// Prepend the requested slice of local recovery rows without dropping remote rows.
///
/// `page` must have been fetched with [`playlist_catalog_window`]. Its remote cursor is translated
/// back into the combined cursor returned to the caller. A one-row remote probe is used when a
/// page is entirely occupied by recovery rows, so more than one page of missing links remains
/// reachable without falsely hiding a following remote page.
pub(super) fn merge_missing_playlist_recovery_rows(
    page: &mut ServerLibraryPage,
    bridge_state: &OpenSubsonicBridgeState,
    offset: u32,
    limit: u32,
) {
    if page.section != ServerLibrarySection::Playlists || limit == 0 {
        return;
    }
    let missing = bridge_state
        .playlist_links()
        .values()
        .filter(|link| link.state == PlaylistLinkState::ServerMissing)
        .map(|link| {
            ServerLibraryRow::Playlist(ServerPlaylistSummary {
                id: link.server_playlist_id.clone(),
                name: link.shadow.name.clone(),
                owner: None,
                song_count: u32::try_from(link.shadow.occurrences.len()).ok(),
                duration_secs: None,
                public: None,
                cover_art_id: None,
                access: ServerPlaylistAccess::Linked,
                link: Some(ServerPlaylistLinkSummary {
                    local_playlist_id: link.local_playlist_id.clone(),
                    health: ServerPlaylistLinkHealth::ServerMissing,
                }),
                readonly_evidence: None,
                owner_evidence: None,
            })
        })
        .collect::<Vec<_>>();
    let missing_count = u32::try_from(missing.len()).unwrap_or(u32::MAX);
    if missing_count == 0 {
        return;
    }
    let missing_start = usize::try_from(offset.min(missing_count)).unwrap_or(usize::MAX);
    let limit_usize = usize::try_from(limit).unwrap_or(usize::MAX);
    let mut rows = missing
        .into_iter()
        .skip(missing_start)
        .take(limit_usize)
        .collect::<Vec<_>>();
    let remote_slots = limit_usize.saturating_sub(rows.len());
    let remote_offset = offset.saturating_sub(missing_count);
    let remote_probe_found_rows = !page.rows.is_empty();
    rows.extend(page.rows.drain(..).take(remote_slots));
    let more_missing =
        offset.saturating_add(u32::try_from(rows.len()).unwrap_or(u32::MAX)) < missing_count;
    page.next_offset = if more_missing {
        Some(offset.saturating_add(limit))
    } else if remote_slots == 0 {
        (remote_probe_found_rows || page.next_offset.is_some())
            .then_some(missing_count.saturating_add(remote_offset))
    } else {
        page.next_offset
            .map(|next| missing_count.saturating_add(next))
    };
    page.rows = rows;
}

fn missing_playlist_link_count(bridge_state: &OpenSubsonicBridgeState) -> u32 {
    u32::try_from(
        bridge_state
            .playlist_links()
            .values()
            .filter(|link| link.state == PlaylistLinkState::ServerMissing)
            .count(),
    )
    .unwrap_or(u32::MAX)
}

fn recovery_prefix_version(bridge_state: &OpenSubsonicBridgeState) -> RecoveryPrefixVersion {
    let mut digest = Sha256::new();
    digest.update(b"yututui-open-subsonic-recovery-prefix-v1\0");
    let mut count = 0_u32;
    for link in bridge_state
        .playlist_links()
        .values()
        .filter(|link| link.state == PlaylistLinkState::ServerMissing)
    {
        count = count.saturating_add(1);
        for part in [
            link.local_playlist_id.as_str().as_bytes(),
            link.server_playlist_id.as_str().as_bytes(),
            link.shadow.name.as_bytes(),
        ] {
            digest.update((part.len() as u64).to_be_bytes());
            digest.update(part);
        }
        digest.update((link.shadow.occurrences.len() as u64).to_be_bytes());
    }
    RecoveryPrefixVersion {
        count,
        revision: digest.finalize().into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::open_subsonic::bridge_store::{PlaylistLink, PlaylistShadow};
    use crate::open_subsonic::model::{AccountScopeId, BackendId, ServerPlaylistId};
    use crate::personal_state::PlaylistId;

    fn state_with_missing(count: usize) -> OpenSubsonicBridgeState {
        let mut state = OpenSubsonicBridgeState::new(
            BackendId::new("backend").unwrap(),
            AccountScopeId::new("account").unwrap(),
        );
        for index in 0..count {
            state
                .upsert_playlist_link(PlaylistLink {
                    local_playlist_id: PlaylistId::new(format!("local-{index:03}")).unwrap(),
                    server_playlist_id: ServerPlaylistId::new(format!("missing-{index:03}"))
                        .unwrap(),
                    managed_by_yututui: true,
                    state: PlaylistLinkState::ServerMissing,
                    content_needs_attention: false,
                    shadow: PlaylistShadow {
                        name: format!("Missing {index}"),
                        occurrences: Vec::new(),
                        verified_at_unix: 100,
                    },
                })
                .unwrap();
        }
        state
    }

    fn remote_row(id: &str) -> ServerLibraryRow {
        ServerLibraryRow::Playlist(ServerPlaylistSummary {
            id: ServerPlaylistId::new(id).unwrap(),
            name: id.to_owned(),
            owner: Some("alice".to_owned()),
            song_count: Some(0),
            duration_secs: None,
            public: None,
            cover_art_id: None,
            access: ServerPlaylistAccess::Server,
            link: None,
            readonly_evidence: Some(false),
            owner_evidence: Some("alice".to_owned()),
        })
    }

    fn page(rows: &[&str], next_offset: Option<u32>) -> ServerLibraryPage {
        ServerLibraryPage {
            section: ServerLibrarySection::Playlists,
            rows: rows.iter().map(|id| remote_row(id)).collect(),
            next_offset,
            warning: None,
        }
    }

    fn row_ids(page: &ServerLibraryPage) -> Vec<&str> {
        page.rows
            .iter()
            .map(|row| match row {
                ServerLibraryRow::Playlist(summary) => summary.id.as_str(),
                _ => unreachable!(),
            })
            .collect()
    }

    #[test]
    fn combined_cursor_preserves_the_trimmed_remote_tail() {
        let state = state_with_missing(1);
        let first_request = ServerLibraryRequest {
            section: ServerLibrarySection::Playlists,
            offset: 0,
            limit: 3,
        };
        assert_eq!(playlist_catalog_window(first_request, &state), (0, 2));
        let mut first = page(&["remote-0", "remote-1"], Some(2));

        merge_missing_playlist_recovery_rows(&mut first, &state, 0, 3);

        assert_eq!(row_ids(&first), ["missing-000", "remote-0", "remote-1"]);
        assert_eq!(first.next_offset, Some(3));
        assert_eq!(
            playlist_catalog_window(
                ServerLibraryRequest {
                    offset: first.next_offset.unwrap(),
                    ..first_request
                },
                &state
            ),
            (2, 3),
            "the next remote page resumes after the two rows actually consumed"
        );
    }

    #[test]
    fn missing_links_larger_than_the_page_remain_reachable_before_remote_rows() {
        let state = state_with_missing(5);
        let request = ServerLibraryRequest {
            section: ServerLibrarySection::Playlists,
            offset: 0,
            limit: 2,
        };
        assert_eq!(playlist_catalog_window(request, &state), (0, 1));

        let mut first = page(&["remote-0"], Some(1));
        merge_missing_playlist_recovery_rows(&mut first, &state, 0, 2);
        assert_eq!(row_ids(&first), ["missing-000", "missing-001"]);
        assert_eq!(first.next_offset, Some(2));

        let mut second = page(&["remote-0"], Some(1));
        merge_missing_playlist_recovery_rows(&mut second, &state, 2, 2);
        assert_eq!(row_ids(&second), ["missing-002", "missing-003"]);
        assert_eq!(second.next_offset, Some(4));

        let mut third = page(&["remote-0"], Some(1));
        merge_missing_playlist_recovery_rows(&mut third, &state, 4, 2);
        assert_eq!(row_ids(&third), ["missing-004", "remote-0"]);
        assert_eq!(third.next_offset, Some(6));
    }

    #[test]
    fn recovery_prefix_change_invalidates_an_issued_combined_offset() {
        let mut state = state_with_missing(3);
        let mut session = PlaylistCatalogSession::default();
        let request = ServerLibraryRequest {
            section: ServerLibrarySection::Playlists,
            offset: 0,
            limit: 2,
        };
        let plan = session.start_page(request, &state).unwrap();
        assert_eq!(plan.remote_window(), (0, 1));
        let mut first = page(&["remote-0"], Some(1));
        session
            .finish_page(&mut first, &state, request, &plan)
            .unwrap();
        assert_eq!(row_ids(&first), ["missing-000", "missing-001"]);
        let stale_offset = first.next_offset.unwrap();

        let local_id = PlaylistId::new("local-000").unwrap();
        let mut recovered = state.playlist_link(&local_id).unwrap().clone();
        recovered.state = PlaylistLinkState::Linked;
        state.upsert_playlist_link(recovered).unwrap();
        assert!(matches!(
            session.start_page(
                ServerLibraryRequest {
                    offset: stale_offset,
                    ..request
                },
                &state
            ),
            Err(ServerError::TemporarilyUnavailable)
        ));

        let restarted = session
            .start_page(
                ServerLibraryRequest {
                    offset: 0,
                    ..request
                },
                &state,
            )
            .unwrap();
        assert_eq!(
            restarted.remote_window(),
            (0, 1),
            "restarting at the first page establishes the new stable recovery prefix"
        );
    }

    #[test]
    fn recovery_prefix_change_during_fetch_rejects_merge_without_rows() {
        let mut state = state_with_missing(2);
        let mut session = PlaylistCatalogSession::default();
        let request = ServerLibraryRequest {
            section: ServerLibrarySection::Playlists,
            offset: 0,
            limit: 2,
        };
        let plan = session.start_page(request, &state).unwrap();
        let mut remote = page(&["remote-0"], Some(1));
        let local_id = PlaylistId::new("local-000").unwrap();
        let mut renamed = state.playlist_link(&local_id).unwrap().clone();
        renamed.shadow.name = "Changed during fetch".to_owned();
        state.upsert_playlist_link(renamed).unwrap();

        assert_eq!(
            session.finish_page(&mut remote, &state, request, &plan),
            Err(ServerError::TemporarilyUnavailable)
        );
        assert_eq!(
            row_ids(&remote),
            ["remote-0"],
            "stale recovery rows must never be combined with the fetched remote page"
        );
    }
}
