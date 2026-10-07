use sha2::{Digest, Sha256};

use super::model::ResumeCandidate;
use super::projection::ListeningProjection;
use crate::api::Song;
use crate::personal_state::{PortableTrack, PortableTrackKey};

pub const AUTO_RESUME_MIN_DURATION_MS: u64 = 20 * 60 * 1_000;
pub const USEFUL_POSITION_MS: u64 = 30 * 1_000;
pub const COMPLETION_MARGIN_MS: u64 = 60 * 1_000;
pub const RESUME_SAVE_INTERVAL_MS: u64 = 60 * 1_000;
pub const PASSPORT_QUALIFY_MS: u64 = 30 * 1_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListeningLoadReason {
    Deliberate,
    SessionRestore,
    Automatic,
    Repeat,
    Bookmark,
    Restart,
    Recovery,
}

impl ListeningLoadReason {
    pub const fn permits_automatic_resume(self) -> bool {
        matches!(self, Self::Deliberate | Self::SessionRestore)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StationVisitTarget {
    pub station_uuid: String,
    pub station_name: String,
    pub country_code: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlaybackMemoryAction {
    SaveResume {
        track: PortableTrack,
        position_ms: u64,
    },
    ClearResume {
        track: PortableTrack,
    },
    RecordPassportVisit(StationVisitTarget),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingListeningSeek {
    pub track_key: PortableTrackKey,
    pub position_ms: u64,
    pub reason: ListeningLoadReason,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PendingSeekOutcome {
    Ready(PendingListeningSeek),
    Invalid(PendingListeningSeek),
}

#[derive(Debug, Clone)]
struct CurrentPlayback {
    track: PortableTrack,
    live: bool,
    station: Option<StationVisitTarget>,
    seekable: Option<bool>,
    duration_ms: Option<u64>,
    position_ms: u64,
    position_confirmed: bool,
    paused: bool,
    buffering: bool,
    last_observed_mono_ms: Option<u64>,
    last_resume_publish_mono_ms: Option<u64>,
    passport_active_ms: u64,
    passport_recorded: bool,
    resume_cleared: bool,
}

impl CurrentPlayback {
    fn resume_eligible(&self) -> bool {
        self.position_confirmed
            && !self.live
            && self.seekable == Some(true)
            && self.duration_ms.is_some_and(|duration| {
                duration >= AUTO_RESUME_MIN_DURATION_MS
                    && self.position_ms >= USEFUL_POSITION_MS
                    && self.position_ms.saturating_add(COMPLETION_MARGIN_MS) < duration
            })
    }

    fn can_seek(&self) -> bool {
        !self.live && self.seekable == Some(true) && self.duration_ms.is_some()
    }
}

#[derive(Debug, Clone)]
pub struct ListeningPlaybackState {
    playback_session_id: String,
    current: Option<CurrentPlayback>,
    pending_seek: Option<PendingListeningSeek>,
    restored_position_ms: Option<u64>,
    active_preset_name: Option<String>,
}

impl Default for ListeningPlaybackState {
    fn default() -> Self {
        Self {
            playback_session_id: format!("playback-{:032x}", fastrand::u128(..)),
            current: None,
            pending_seek: None,
            restored_position_ms: None,
            active_preset_name: None,
        }
    }
}

impl ListeningPlaybackState {
    pub fn playback_session_id(&self) -> &str {
        &self.playback_session_id
    }

    pub fn begin_track(
        &mut self,
        track: PortableTrack,
        live: bool,
        station: Option<StationVisitTarget>,
    ) {
        if self
            .pending_seek
            .as_ref()
            .is_some_and(|pending| pending.track_key != track.key)
        {
            self.pending_seek = None;
        }
        self.restored_position_ms = None;
        self.current = Some(CurrentPlayback {
            track,
            live,
            station,
            seekable: None,
            duration_ms: None,
            position_ms: 0,
            position_confirmed: false,
            paused: false,
            buffering: false,
            last_observed_mono_ms: None,
            last_resume_publish_mono_ms: None,
            passport_active_ms: 0,
            passport_recorded: false,
            resume_cleared: false,
        });
    }

    pub fn clear_track(&mut self) {
        self.current = None;
        self.pending_seek = None;
        self.restored_position_ms = None;
    }

    pub fn current_track(&self) -> Option<&PortableTrack> {
        Some(&self.current.as_ref()?.track)
    }

    pub fn current_position_ms(&self) -> Option<u64> {
        let current = self.current.as_ref()?;
        current.position_confirmed.then_some(current.position_ms)
    }

    pub fn rebind_current_track(&mut self, track: PortableTrack) {
        if let Some(current) = self.current.as_mut() {
            current.track = track;
        }
    }

    pub fn observe_position_only(&mut self, position: f64, monotonic_ms: u64) {
        let Some(current) = self.current.as_mut() else {
            return;
        };
        let Some(position_ms) = seconds_to_ms(position) else {
            return;
        };
        current.position_ms = position_ms;
        current.position_confirmed = true;
        current.last_observed_mono_ms = Some(monotonic_ms);
    }

    pub fn current_can_seek(&self) -> bool {
        self.current.as_ref().is_some_and(CurrentPlayback::can_seek)
    }

    pub fn resume_position_eligible(&self, position_ms: u64) -> bool {
        self.current.as_ref().is_some_and(|current| {
            current.resume_eligible()
                || current.duration_ms.is_some_and(|duration| {
                    !current.live
                        && current.seekable == Some(true)
                        && duration >= AUTO_RESUME_MIN_DURATION_MS
                        && position_ms >= USEFUL_POSITION_MS
                        && position_ms.saturating_add(COMPLETION_MARGIN_MS) < duration
                })
        })
    }

    pub fn observe_seekable(&mut self, seekable: Option<bool>) {
        if let Some(current) = self.current.as_mut() {
            current.seekable = seekable;
        }
    }

    pub fn observe_duration_secs(&mut self, duration: Option<f64>) {
        if let Some(current) = self.current.as_mut() {
            current.duration_ms = duration.and_then(seconds_to_ms);
        }
    }

    pub fn observe_buffering(&mut self, buffering: bool) {
        if let Some(current) = self.current.as_mut() {
            if current.buffering != buffering {
                current.last_observed_mono_ms = None;
            }
            current.buffering = buffering;
        }
    }

    pub fn observe_paused(
        &mut self,
        paused: bool,
        automatic_resume_enabled: bool,
    ) -> Option<PlaybackMemoryAction> {
        let current = self.current.as_mut()?;
        let became_paused = paused && !current.paused;
        if current.paused != paused {
            current.last_observed_mono_ms = None;
        }
        current.paused = paused;
        let action = became_paused
            .then(|| resume_snapshot(current, automatic_resume_enabled))
            .flatten();
        if matches!(action, Some(PlaybackMemoryAction::ClearResume { .. })) {
            current.resume_cleared = true;
        }
        action
    }

    pub fn observe_position_secs(
        &mut self,
        position: f64,
        monotonic_ms: u64,
        automatic_resume_enabled: bool,
    ) -> Vec<PlaybackMemoryAction> {
        let Some(current) = self.current.as_mut() else {
            return Vec::new();
        };
        let Some(position_ms) = seconds_to_ms(position) else {
            return Vec::new();
        };
        let previous_position = current.position_ms;
        let previous_confirmed = current.position_confirmed;
        current.position_ms = position_ms;
        current.position_confirmed = true;

        let elapsed = current
            .last_observed_mono_ms
            .replace(monotonic_ms)
            .map(|previous| monotonic_ms.saturating_sub(previous))
            .unwrap_or(0)
            .min(5_000);
        let progressing = previous_confirmed
            && position_ms > previous_position
            && position_ms.saturating_sub(previous_position) <= elapsed.saturating_add(2_000);
        if progressing && !current.paused && !current.buffering {
            current.passport_active_ms = current.passport_active_ms.saturating_add(elapsed);
        }

        let mut actions = Vec::with_capacity(2);
        if !current.passport_recorded
            && current.passport_active_ms >= PASSPORT_QUALIFY_MS
            && let Some(station) = current.station.clone()
        {
            current.passport_recorded = true;
            actions.push(PlaybackMemoryAction::RecordPassportVisit(station));
        }

        let publish_due = current
            .last_resume_publish_mono_ms
            .is_none_or(|last| monotonic_ms.saturating_sub(last) >= RESUME_SAVE_INTERVAL_MS);
        if publish_due && let Some(action) = resume_snapshot(current, automatic_resume_enabled) {
            current.last_resume_publish_mono_ms = Some(monotonic_ms);
            current.resume_cleared = matches!(action, PlaybackMemoryAction::ClearResume { .. });
            actions.push(action);
        }
        actions
    }

    pub fn snapshot_resume(&self, automatic_resume_enabled: bool) -> Option<PlaybackMemoryAction> {
        resume_snapshot(self.current.as_ref()?, automatic_resume_enabled)
    }

    pub fn complete_track(&mut self) -> Option<PlaybackMemoryAction> {
        let current = self.current.take()?;
        self.pending_seek = None;
        self.restored_position_ms = None;
        (!current.live).then_some(PlaybackMemoryAction::ClearResume {
            track: current.track,
        })
    }

    pub fn request_seek(
        &mut self,
        track_key: PortableTrackKey,
        position_ms: u64,
        reason: ListeningLoadReason,
    ) {
        self.pending_seek = Some(PendingListeningSeek {
            track_key,
            position_ms,
            reason,
        });
    }

    pub fn has_pending_seek(&self) -> bool {
        self.pending_seek.is_some()
    }

    pub fn cancel_pending_seek(&mut self) {
        self.pending_seek = None;
    }

    pub fn pending_seek_outcome(&self) -> Option<PendingSeekOutcome> {
        let current = self.current.as_ref()?;
        let pending = self.pending_seek.as_ref()?;
        if pending.track_key != current.track.key {
            return None;
        }
        if current.live || current.seekable == Some(false) {
            return Some(PendingSeekOutcome::Invalid(pending.clone()));
        }
        if current.seekable.is_none() || current.duration_ms.is_none() {
            return None;
        }
        let duration = current
            .duration_ms
            .expect("seekable finite media has duration");
        let valid = match pending.reason {
            ListeningLoadReason::Deliberate | ListeningLoadReason::SessionRestore => {
                duration >= AUTO_RESUME_MIN_DURATION_MS
                    && pending.position_ms >= USEFUL_POSITION_MS
                    && pending.position_ms.saturating_add(COMPLETION_MARGIN_MS) < duration
            }
            ListeningLoadReason::Bookmark | ListeningLoadReason::Recovery => {
                pending.position_ms < duration
            }
            ListeningLoadReason::Restart => pending.position_ms == 0,
            ListeningLoadReason::Automatic | ListeningLoadReason::Repeat => {
                pending.position_ms == 0
            }
        };
        let pending = pending.clone();
        Some(if valid {
            PendingSeekOutcome::Ready(pending)
        } else {
            PendingSeekOutcome::Invalid(pending)
        })
    }

    pub fn take_ready_seek(&mut self) -> Option<PendingListeningSeek> {
        match self.pending_seek_outcome()? {
            PendingSeekOutcome::Ready(pending) => {
                self.pending_seek = None;
                Some(pending)
            }
            PendingSeekOutcome::Invalid(_) => {
                self.pending_seek = None;
                None
            }
        }
    }

    pub fn note_restored_position(&mut self, position_ms: u64) {
        self.restored_position_ms = Some(position_ms);
        if let Some(current) = self.current.as_mut() {
            current.position_ms = position_ms;
            current.position_confirmed = false;
            current.last_observed_mono_ms = None;
        }
    }

    pub fn restored_position_ms(&self) -> Option<u64> {
        self.restored_position_ms
    }

    pub fn set_active_preset_name(&mut self, name: Option<String>) {
        self.active_preset_name = name;
    }

    pub fn active_preset_name(&self) -> Option<&str> {
        self.active_preset_name.as_deref()
    }
}

fn resume_snapshot(
    current: &CurrentPlayback,
    automatic_resume_enabled: bool,
) -> Option<PlaybackMemoryAction> {
    if !automatic_resume_enabled
        || !current.position_confirmed
        || current.live
        || current.seekable != Some(true)
    {
        return None;
    }
    if current.duration_ms.is_some_and(|duration| {
        duration >= AUTO_RESUME_MIN_DURATION_MS
            && current.position_ms >= USEFUL_POSITION_MS
            && current.position_ms.saturating_add(COMPLETION_MARGIN_MS) >= duration
    }) {
        return (!current.resume_cleared).then(|| PlaybackMemoryAction::ClearResume {
            track: current.track.clone(),
        });
    }
    if !current.resume_eligible() {
        return None;
    }
    Some(PlaybackMemoryAction::SaveResume {
        track: current.track.clone(),
        position_ms: current.position_ms,
    })
}

fn seconds_to_ms(seconds: f64) -> Option<u64> {
    (seconds.is_finite() && seconds >= 0.0)
        .then(|| (seconds * 1_000.0).round().clamp(0.0, u64::MAX as f64) as u64)
}

pub fn portable_track(song: &Song, local_scope: Option<&str>) -> PortableTrack {
    let pure_local = song.is_local() && song.youtube_id().is_none();
    let key = if let Some(item) = song.open_subsonic_item() {
        PortableTrackKey::OpenSubsonic {
            backend_id: item.backend_id().as_str().to_owned(),
            account_scope_id: item.account_scope_id().as_str().to_owned(),
            item_id: item.item_id().as_str().to_owned(),
        }
    } else if let Some(youtube_id) = song.youtube_id() {
        PortableTrackKey::Catalog {
            provider: "youtube".to_owned(),
            exact_catalog_id: youtube_id.to_owned(),
        }
    } else if song.is_local() {
        let mut digest = Sha256::new();
        digest.update(b"ytt-listening-local-v1\0");
        digest.update(song.video_id.as_bytes());
        let digest = format!("{:x}", digest.finalize());
        PortableTrackKey::LocalPlaceholder {
            portable_placeholder_id: local_scope.map_or_else(
                || format!("unbound:{digest}"),
                |scope| format!("{}{digest}", local_placeholder_prefix(scope)),
            ),
        }
    } else {
        PortableTrackKey::Catalog {
            provider: song.source.id_prefix().to_owned(),
            exact_catalog_id: song.video_id.clone(),
        }
    };
    PortableTrack {
        key,
        title: if pure_local {
            local_safe_label(&song.title, "Local track")
        } else {
            song.title.clone()
        },
        artist: if pure_local {
            local_safe_label(&song.artist, "Local file")
        } else {
            song.artist.clone()
        },
        album: (!pure_local).then(|| song.album.clone()).flatten(),
        duration_secs: song
            .duration_secs
            .or_else(|| crate::streaming::candidate::parse_duration_secs(&song.duration)),
        isrc: (!pure_local).then(|| song.isrc.clone()).flatten(),
    }
}

fn local_safe_label(value: &str, fallback: &str) -> String {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return fallback.to_owned();
    }
    if trimmed.contains('/') || trimmed.contains('\\') {
        return std::path::Path::new(trimmed)
            .file_stem()
            .and_then(|name| name.to_str())
            .filter(|name| !name.trim().is_empty())
            .unwrap_or(fallback)
            .to_owned();
    }
    trimmed.to_owned()
}

pub fn radio_station_target(song: &Song) -> Option<StationVisitTarget> {
    if !song.is_radio_station() {
        return None;
    }
    let station_uuid = song.video_id.strip_prefix("rad:")?.to_owned();
    (!station_uuid.is_empty()).then(|| StationVisitTarget {
        station_uuid,
        station_name: song.title.clone(),
        country_code: song.radio_country_code.clone(),
    })
}

pub fn format_listening_projection(projection: &ListeningProjection) -> String {
    const ROW_LIMIT: usize = 256;
    let mut rows = Vec::new();
    for (id, revisions) in &projection.bookmarks {
        for bookmark in revisions {
            rows.push(format!(
                "bookmark\t{}\t{}ms\t{}\t{}",
                id.as_str(),
                bookmark.position_ms,
                wire_label(&bookmark.label),
                wire_label(&bookmark.track.title),
            ));
        }
    }
    for (id, revisions) in &projection.dj_presets {
        for preset in revisions {
            rows.push(format!(
                "preset\t{}\t{}",
                id.as_str(),
                wire_label(&preset.name)
            ));
        }
    }
    for (uuid, visit) in &projection.passport_visits {
        rows.push(format!(
            "passport\t{}\t{}\t{}",
            wire_label(uuid),
            visit.country_code.as_deref().unwrap_or("--"),
            wire_label(&visit.station_name),
        ));
    }
    for state in projection.resumes.values() {
        for candidate in &state.candidates {
            match candidate {
                ResumeCandidate::Position(point) => rows.push(format!(
                    "resume\t{}ms\t{}",
                    point.position_ms,
                    wire_label(&point.track.title)
                )),
                ResumeCandidate::Clear(clear) => {
                    rows.push(format!("resume-clear\t{}", wire_label(&clear.track.title)))
                }
            }
        }
    }
    let omitted = rows.len().saturating_sub(ROW_LIMIT);
    rows.truncate(ROW_LIMIT);
    if omitted > 0 {
        rows.push(format!("… {omitted} more records"));
    }
    if rows.is_empty() {
        "No listening records".to_owned()
    } else {
        rows.join("\n")
    }
}

fn wire_label(value: &str) -> String {
    value
        .chars()
        .filter(|ch| !ch.is_control())
        .take(160)
        .collect()
}

pub fn resolve_portable_track(track: &PortableTrack, local_scope: Option<&str>) -> Option<Song> {
    let (video_id, source, playable) = match &track.key {
        PortableTrackKey::Catalog {
            provider,
            exact_catalog_id,
        } if matches!(provider.as_str(), "youtube" | "yt") => (
            exact_catalog_id.clone(),
            crate::search_source::SearchSource::Youtube,
            None,
        ),
        PortableTrackKey::Catalog { .. } => return None,
        PortableTrackKey::OpenSubsonic {
            backend_id,
            account_scope_id,
            item_id,
        } => {
            use crate::open_subsonic::{AccountScopeId, BackendId, ItemId, OpenSubsonicItemRef};

            let item = OpenSubsonicItemRef::new(
                BackendId::new(backend_id.clone()).ok()?,
                AccountScopeId::new(account_scope_id.clone()).ok()?,
                ItemId::new(item_id.clone()).ok()?,
            );
            (
                item.stable_track_id(),
                crate::search_source::SearchSource::OpenSubsonic,
                Some(crate::api::PlayableRef::OpenSubsonic {
                    item,
                    cover_art_id: None,
                }),
            )
        }
        PortableTrackKey::LocalPlaceholder {
            portable_placeholder_id,
        } => {
            let scope = local_scope?;
            let prefix = local_placeholder_prefix(scope);
            if !portable_placeholder_id.starts_with(&prefix) {
                return None;
            }
            return None;
        }
    };
    let duration = track.duration_secs.map_or_else(String::new, |seconds| {
        format!("{}:{:02}", seconds / 60, seconds % 60)
    });
    let mut song = Song::remote(
        video_id,
        track.title.clone(),
        track.artist.clone(),
        duration,
    );
    song.source = source;
    song.playable = playable;
    song.album.clone_from(&track.album);
    song.duration_secs = track.duration_secs;
    song.isrc.clone_from(&track.isrc);
    Some(song)
}

fn local_placeholder_prefix(scope: &str) -> String {
    let mut digest = Sha256::new();
    digest.update(b"ytt-listening-local-owner-v1\0");
    digest.update(scope.as_bytes());
    format!("{:x}:", digest.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track() -> PortableTrack {
        PortableTrack {
            key: PortableTrackKey::Catalog {
                provider: "youtube".to_owned(),
                exact_catalog_id: "long".to_owned(),
            },
            title: "Long form".to_owned(),
            artist: "Artist".to_owned(),
            album: None,
            duration_secs: Some(1_800),
            isrc: None,
        }
    }

    #[test]
    fn resume_requires_confirmed_seekability_and_bounded_position() {
        let mut state = ListeningPlaybackState::default();
        state.begin_track(track(), false, None);
        state.observe_duration_secs(Some(1_800.0));
        assert!(state.observe_position_secs(120.0, 60_000, true).is_empty());
        state.observe_seekable(Some(true));
        assert!(matches!(
            state.observe_position_secs(121.0, 120_000, true).as_slice(),
            [PlaybackMemoryAction::SaveResume {
                position_ms: 121_000,
                ..
            }]
        ));
        assert!(matches!(
            state
                .observe_position_secs(1_750.0, 180_000, true)
                .as_slice(),
            [PlaybackMemoryAction::ClearResume { .. }]
        ));
    }

    #[test]
    fn passport_counts_only_active_confirmed_progress() {
        let mut state = ListeningPlaybackState::default();
        state.begin_track(
            track(),
            true,
            Some(StationVisitTarget {
                station_uuid: "station".to_owned(),
                station_name: "Station".to_owned(),
                country_code: Some("KR".to_owned()),
            }),
        );
        state.observe_position_secs(1.0, 1_000, true);
        state.observe_buffering(true);
        assert!(state.observe_position_secs(21.0, 21_000, true).is_empty());
        state.observe_buffering(false);
        state.observe_paused(true, true);
        assert!(state.observe_position_secs(31.0, 31_000, true).is_empty());
        state.observe_paused(false, true);
        assert!(state.observe_position_secs(41.0, 41_000, true).is_empty());
        assert!(state.observe_position_secs(46.0, 46_000, true).is_empty());
        assert!(state.observe_position_secs(51.0, 51_000, true).is_empty());
        assert!(state.observe_position_secs(56.0, 56_000, true).is_empty());
        assert!(state.observe_position_secs(61.0, 61_000, true).is_empty());
        assert!(state.observe_position_secs(66.0, 66_000, true).is_empty());
        assert!(matches!(
            state.observe_position_secs(71.0, 71_000, true).as_slice(),
            [PlaybackMemoryAction::RecordPassportVisit(_)]
        ));
    }

    #[test]
    fn pending_seek_cancels_on_a_different_track() {
        let mut state = ListeningPlaybackState::default();
        let first = track();
        state.request_seek(first.key.clone(), 90_000, ListeningLoadReason::Bookmark);
        let mut other = first;
        other.key = PortableTrackKey::Catalog {
            provider: "youtube".to_owned(),
            exact_catalog_id: "other".to_owned(),
        };
        state.begin_track(other, false, None);
        state.observe_duration_secs(Some(1_800.0));
        state.observe_seekable(Some(true));
        assert!(state.take_ready_seek().is_none());
    }

    #[test]
    fn admitted_restart_replaces_the_confirmed_position() {
        let mut state = ListeningPlaybackState::default();
        state.begin_track(track(), false, None);
        state.observe_duration_secs(Some(1_800.0));
        state.observe_seekable(Some(true));
        state.observe_position_only(600.0, 1_000);

        state.note_restored_position(0);

        assert_eq!(state.current_position_ms(), None);
        assert!(state.snapshot_resume(true).is_none());
    }

    #[test]
    fn automatic_resume_rejects_a_point_outside_the_current_duration() {
        let mut state = ListeningPlaybackState::default();
        let track = track();
        state.request_seek(track.key.clone(), 600_000, ListeningLoadReason::Deliberate);
        state.begin_track(track, false, None);
        state.observe_duration_secs(Some(300.0));
        state.observe_seekable(Some(true));

        assert!(matches!(
            state.pending_seek_outcome(),
            Some(PendingSeekOutcome::Invalid(_))
        ));
        state.cancel_pending_seek();
        assert!(!state.has_pending_seek());
    }

    #[test]
    fn local_identity_survives_causal_device_enrollment() {
        let song = Song::local_file("/Users/alice/Music/private-talk.mp3".into());
        let before_enrollment = portable_track(&song, Some("persisted-local-scope"));
        let after_enrollment = portable_track(&song, Some("persisted-local-scope"));

        assert_eq!(before_enrollment.key, after_enrollment.key);
        assert!(!before_enrollment.title.contains("/Users/alice"));
    }

    #[test]
    fn equal_machine_paths_do_not_match_across_local_scopes() {
        let song = Song::local_file("/Users/alice/Music/private-talk.mp3".into());
        let first = portable_track(&song, Some("device-private-scope-a"));
        let second = portable_track(&song, Some("device-private-scope-b"));

        assert_ne!(first.key, second.key);
    }
}
