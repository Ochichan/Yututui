use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use super::legacy::{
    ENGAGEMENT_EVENTS_MAX, FAVORITES_MAX, HISTORY_MAX, LegacyPlayEvent, LegacyPlaylist,
    LegacyPlaylistEntry, LegacyProjection, LegacySignals, LegacyStation, LegacyTrackSignal,
    PLAYLIST_ENTRIES_MAX, PLAYLISTS_MAX, RADIO_MAX, SIGNAL_TRACKS_MAX, rating_from_legacy,
    sha256_hex,
};
use super::model::operation_set;
use super::{
    CausalStamp, DeviceRecord, DeviceRegistry, EngagementKind, Operation, OperationEnvelope,
    PersonalStateError, PersonalStateV2, PlaylistEntryId, PlaylistId, PortableTrack,
    PortableTrackKey, Rating, VersionVector,
};

const RAW_EVENT_RETENTION_SECS: i64 = 365 * 24 * 60 * 60;
const ARTIST_WEIGHT_MIN: f32 = -2.0;
const ARTIST_WEIGHT_MAX: f32 = 2.0;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MergeSummary {
    pub local_operations: usize,
    pub remote_operations: usize,
    pub added_operations: usize,
    pub duplicate_operations: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PersonalProjection {
    pub legacy: LegacyProjection,
    pub device_registry: DeviceRegistry,
    pub version_vector: VersionVector,
    pub revision: u64,
    pub fingerprint: String,
    pub repaired_playlist_cycles: usize,
}

impl PersonalProjection {
    pub fn into_runtime(
        self,
    ) -> (
        crate::library::Library,
        crate::playlists::Playlists,
        crate::signals::Signals,
        crate::station::StationStore,
    ) {
        self.legacy.into_runtime()
    }
}

#[derive(Clone)]
struct Register<T> {
    stamp: CausalStamp,
    operation_id: String,
    from_baseline: bool,
    value: T,
}

impl<T> Register<T> {
    fn new(envelope: &OperationEnvelope, value: T) -> Self {
        Self {
            stamp: envelope.stamp.clone(),
            operation_id: envelope.operation_id.clone(),
            from_baseline: false,
            value,
        }
    }

    fn baseline(envelope: &OperationEnvelope, value: T) -> Self {
        Self {
            stamp: envelope.stamp.clone(),
            operation_id: envelope.operation_id.clone(),
            from_baseline: true,
            value,
        }
    }

    fn accepts(&self, envelope: &OperationEnvelope) -> bool {
        if self.from_baseline {
            return true;
        }
        stamp_order(
            &envelope.stamp,
            &envelope.operation_id,
            &self.stamp,
            &self.operation_id,
        )
        .is_gt()
    }

    fn update(&mut self, envelope: &OperationEnvelope, value: T) {
        if self.accepts(envelope) {
            *self = Self::new(envelope, value);
        }
    }
}

#[derive(Clone)]
struct PlaylistRegisters {
    slug: String,
    name: Register<String>,
    deleted: Register<bool>,
    entries: BTreeMap<PlaylistEntryId, EntryRegisters>,
}

#[derive(Clone)]
struct EntryRegisters {
    track: Register<PortableTrack>,
    removed: Register<bool>,
    position: Register<Option<PlaylistEntryId>>,
}

#[derive(Clone)]
struct EngagementEvent {
    envelope: OperationEnvelope,
    event_id: String,
    track: PortableTrack,
    engagement: EngagementKind,
    played_duration_ms: Option<u64>,
    total_duration_ms: Option<u64>,
    artist_key: String,
}

pub fn merge(
    local: &PersonalStateV2,
    remote: &PersonalStateV2,
) -> Result<(PersonalStateV2, MergeSummary), PersonalStateError> {
    local.validate()?;
    remote.validate()?;
    if local.dataset_id != remote.dataset_id {
        return Err(PersonalStateError::InvalidOperation(
            "different datasets require an explicit import",
        ));
    }

    let mut merged = local.clone();
    let local_ids = operation_set(local);
    let mut operations = local
        .operations
        .iter()
        .map(|operation| (operation.operation_id.clone(), operation.clone()))
        .collect::<BTreeMap<_, _>>();
    let mut summary = MergeSummary {
        local_operations: local.operations.len(),
        remote_operations: remote.operations.len(),
        ..MergeSummary::default()
    };
    for operation in &remote.operations {
        match operations.get(&operation.operation_id) {
            Some(existing) if existing == operation => {
                summary.duplicate_operations += 1;
            }
            Some(_) => return Err(PersonalStateError::ConflictingOperationId),
            None => {
                if local_ids.contains(operation.operation_id.as_str()) {
                    summary.duplicate_operations += 1;
                } else {
                    summary.added_operations += 1;
                }
                operations.insert(operation.operation_id.clone(), operation.clone());
            }
        }
    }
    merged.operations = operations.into_values().collect();
    merged.version_vector.merge(&remote.version_vector);
    merge_device_registry(&mut merged.device_registry, &remote.device_registry);
    merged.compaction_checkpoint = choose_checkpoint(
        merged.compaction_checkpoint.as_ref(),
        remote.compaction_checkpoint.as_ref(),
    );
    merged.revision = local.revision.max(remote.revision);
    merged.projection_fingerprint = None;
    merged.normalize()?;
    Ok((merged, summary))
}

pub fn project(state: &PersonalStateV2) -> Result<PersonalProjection, PersonalStateError> {
    project_at(state, crate::signals::unix_now())
}

pub(crate) fn project_at(
    state: &PersonalStateV2,
    now_unix: i64,
) -> Result<PersonalProjection, PersonalStateError> {
    state.validate()?;

    let baseline = winning_baseline(&state.operations);
    let mut legacy = baseline
        .as_ref()
        .map(|(_, baseline)| baseline.clone())
        .unwrap_or_else(empty_legacy);
    let baseline_ratings = rating_from_legacy(&legacy.favorites, &legacy.signals);

    let mut ratings = BTreeMap::<PortableTrackKey, Register<(PortableTrack, Rating)>>::new();
    let mut radio = BTreeMap::<PortableTrackKey, Register<(PortableTrack, bool)>>::new();
    let mut playlists = BTreeMap::<PlaylistId, PlaylistRegisters>::new();
    let mut station_profile = None::<Register<(Option<String>, crate::station::Explore)>>;
    let mut avoided = BTreeMap::<String, Register<bool>>::new();
    let mut events = BTreeMap::<String, EngagementEvent>::new();
    let mut devices = state.device_registry.clone();
    let mut bindings = BTreeMap::<PortableTrackKey, Register<PortableTrackKey>>::new();

    if let Some((baseline_envelope, baseline_value)) = &baseline {
        for (key, (track, rating)) in
            rating_from_legacy(&baseline_value.favorites, &baseline_value.signals)
        {
            ratings.insert(key, Register::baseline(baseline_envelope, (track, rating)));
        }
        for station in &baseline_value.radio_favorites {
            radio.insert(
                station.key.clone(),
                Register::baseline(baseline_envelope, (station.clone(), true)),
            );
        }
        for playlist in &baseline_value.playlists {
            let mut entry_registers = BTreeMap::new();
            let mut previous = None;
            for entry in &playlist.entries {
                entry_registers.insert(
                    entry.entry_id.clone(),
                    EntryRegisters {
                        track: Register::baseline(baseline_envelope, entry.track.clone()),
                        removed: Register::baseline(baseline_envelope, false),
                        position: Register::baseline(baseline_envelope, previous.clone()),
                    },
                );
                previous = Some(entry.entry_id.clone());
            }
            playlists.insert(
                playlist.playlist_id.clone(),
                PlaylistRegisters {
                    slug: playlist.slug.clone(),
                    name: Register::baseline(baseline_envelope, playlist.name.clone()),
                    deleted: Register::baseline(baseline_envelope, false),
                    entries: entry_registers,
                },
            );
        }
        station_profile = Some(Register::baseline(
            baseline_envelope,
            (
                baseline_value.station.query.clone(),
                baseline_value.station.explore,
            ),
        ));
        for artist_key in &baseline_value.station.avoid_artist_keys {
            avoided.insert(
                artist_key.clone(),
                Register::baseline(baseline_envelope, true),
            );
        }
    }

    for envelope in &state.operations {
        match &envelope.operation {
            Operation::SetRating { track, rating } => {
                update_register(
                    &mut ratings,
                    track.key.clone(),
                    envelope,
                    (track.clone(), *rating),
                );
            }
            Operation::RecordEngagement {
                event_id,
                track,
                engagement,
                played_duration_ms,
                total_duration_ms,
                artist_key,
            } => {
                let candidate = EngagementEvent {
                    envelope: envelope.clone(),
                    event_id: event_id.clone(),
                    track: track.clone(),
                    engagement: *engagement,
                    played_duration_ms: *played_duration_ms,
                    total_duration_ms: *total_duration_ms,
                    artist_key: artist_key.clone(),
                };
                match events.get(event_id) {
                    Some(current)
                        if stamp_order(
                            &candidate.envelope.stamp,
                            &candidate.envelope.operation_id,
                            &current.envelope.stamp,
                            &current.envelope.operation_id,
                        )
                        .is_gt() =>
                    {
                        events.insert(event_id.clone(), candidate);
                    }
                    None => {
                        events.insert(event_id.clone(), candidate);
                    }
                    _ => {}
                }
            }
            Operation::SetRadioFavorite { station, favorite } => {
                update_register(
                    &mut radio,
                    station.key.clone(),
                    envelope,
                    (station.clone(), *favorite),
                );
            }
            Operation::UpsertPlaylist { playlist_id, name } => {
                let playlist = playlists
                    .entry(playlist_id.clone())
                    .or_insert_with(|| empty_playlist(envelope, playlist_id, name));
                playlist.name.update(envelope, name.clone());
                playlist.deleted.update(envelope, false);
            }
            Operation::DeletePlaylist {
                playlist_id,
                deleted,
            } => {
                let playlist = playlists
                    .entry(playlist_id.clone())
                    .or_insert_with(|| empty_playlist(envelope, playlist_id, "Playlist"));
                playlist.deleted.update(envelope, *deleted);
            }
            Operation::UpsertPlaylistEntry {
                playlist_id,
                entry_id,
                track,
                after_entry_id,
            } => {
                let playlist = playlists
                    .entry(playlist_id.clone())
                    .or_insert_with(|| empty_playlist(envelope, playlist_id, "Playlist"));
                playlist.deleted.update(envelope, false);
                let entry =
                    playlist
                        .entries
                        .entry(entry_id.clone())
                        .or_insert_with(|| EntryRegisters {
                            track: Register::new(envelope, track.clone()),
                            removed: Register::new(envelope, false),
                            position: Register::new(envelope, after_entry_id.clone()),
                        });
                entry.track.update(envelope, track.clone());
                entry.removed.update(envelope, false);
                entry.position.update(envelope, after_entry_id.clone());
            }
            Operation::MovePlaylistEntry {
                playlist_id,
                entry_id,
                after_entry_id,
            } => {
                if let Some(entry) = playlists
                    .get_mut(playlist_id)
                    .and_then(|playlist| playlist.entries.get_mut(entry_id))
                {
                    entry.position.update(envelope, after_entry_id.clone());
                }
            }
            Operation::RemovePlaylistEntry {
                playlist_id,
                entry_id,
                removed,
            } => {
                if let Some(entry) = playlists
                    .get_mut(playlist_id)
                    .and_then(|playlist| playlist.entries.get_mut(entry_id))
                {
                    entry.removed.update(envelope, *removed);
                }
            }
            Operation::SetStationProfile { query, explore } => {
                update_optional_register(&mut station_profile, envelope, (query.clone(), *explore));
            }
            Operation::SetAvoidArtist { artist_key, avoid } => {
                update_register(&mut avoided, artist_key.clone(), envelope, *avoid);
            }
            Operation::BindTrack {
                placeholder,
                target,
            } => {
                update_register(&mut bindings, placeholder.clone(), envelope, target.clone());
            }
            Operation::AddDevice { device } => {
                let entry = devices
                    .entry(device.device_id.clone())
                    .or_insert_with(|| device.clone());
                if !entry.revoked {
                    *entry = device.clone();
                }
            }
            Operation::RevokeDevice { device_id } => {
                devices
                    .entry(device_id.clone())
                    .and_modify(|device| device.revoked = true)
                    .or_insert_with(|| DeviceRecord {
                        device_id: device_id.clone(),
                        name: "Revoked device".to_owned(),
                        revoked: true,
                    });
            }
            Operation::LegacyBaseline { .. } => {}
        }
    }

    apply_track_bindings(&mut ratings, &mut radio, &bindings);
    legacy.favorites = projected_favorites(&ratings, Rating::Liked);
    apply_disliked_projection(&mut legacy.signals, &ratings);
    legacy.radio_favorites = projected_radio(&radio);

    let retained_events = retained_events(events.into_values(), now_unix);
    apply_engagement_projection(&mut legacy, &retained_events);
    apply_rating_affinity_delta(&mut legacy.signals, &baseline_ratings, &ratings);

    let (projected_playlists, repaired_playlist_cycles) = project_playlists(playlists);
    legacy.playlists = projected_playlists;
    legacy.station = LegacyStation {
        query: station_profile
            .as_ref()
            .and_then(|register| register.value.0.clone()),
        explore: station_profile
            .as_ref()
            .map(|register| register.value.1)
            .unwrap_or_default(),
        avoid_artist_keys: avoided
            .into_iter()
            .filter_map(|(artist, register)| register.value.then_some(artist))
            .take(200)
            .collect(),
    };
    enforce_projection_caps(&mut legacy);

    let fingerprint = projection_fingerprint(&legacy)?;
    Ok(PersonalProjection {
        legacy,
        device_registry: devices,
        version_vector: state.version_vector.clone(),
        revision: state.revision,
        fingerprint,
        repaired_playlist_cycles,
    })
}

pub(crate) fn runtime_fingerprint(
    library: &crate::library::Library,
    playlists: &crate::playlists::Playlists,
    signals: &crate::signals::Signals,
    station: &crate::station::StationStore,
) -> Result<String, PersonalStateError> {
    projection_fingerprint(&LegacyProjection::from_runtime(
        library, playlists, signals, station,
    ))
}

fn winning_baseline(
    operations: &[OperationEnvelope],
) -> Option<(OperationEnvelope, LegacyProjection)> {
    let mut winner = None::<(OperationEnvelope, LegacyProjection)>;
    for envelope in operations {
        let Operation::LegacyBaseline { baseline } = &envelope.operation else {
            continue;
        };
        let replace = winner.as_ref().is_none_or(|(current, _)| {
            stamp_order(
                &envelope.stamp,
                &envelope.operation_id,
                &current.stamp,
                &current.operation_id,
            )
            .is_gt()
        });
        if replace {
            winner = Some((envelope.clone(), baseline.as_ref().clone()));
        }
    }
    winner
}

fn update_register<K: Ord, T>(
    registers: &mut BTreeMap<K, Register<T>>,
    key: K,
    envelope: &OperationEnvelope,
    value: T,
) {
    match registers.get_mut(&key) {
        Some(register) => register.update(envelope, value),
        None => {
            registers.insert(key, Register::new(envelope, value));
        }
    }
}

fn update_optional_register<T>(
    register: &mut Option<Register<T>>,
    envelope: &OperationEnvelope,
    value: T,
) {
    match register {
        Some(register) => register.update(envelope, value),
        None => *register = Some(Register::new(envelope, value)),
    }
}

fn stamp_order(
    candidate: &CausalStamp,
    candidate_id: &str,
    current: &CausalStamp,
    current_id: &str,
) -> Ordering {
    if candidate.happens_after(current) {
        Ordering::Greater
    } else if current.happens_after(candidate) {
        Ordering::Less
    } else {
        candidate
            .dot
            .cmp(&current.dot)
            .then(candidate_id.cmp(current_id))
    }
}

fn projected_favorites(
    ratings: &BTreeMap<PortableTrackKey, Register<(PortableTrack, Rating)>>,
    expected: Rating,
) -> Vec<PortableTrack> {
    let mut values = ratings
        .values()
        .filter(|register| register.value.1 == expected)
        .collect::<Vec<_>>();
    values.sort_by(|left, right| {
        right
            .stamp
            .dot
            .cmp(&left.stamp.dot)
            .then(left.value.0.key.cmp(&right.value.0.key))
    });
    values
        .into_iter()
        .take(FAVORITES_MAX)
        .map(|register| register.value.0.clone())
        .collect()
}

fn projected_radio(
    radio: &BTreeMap<PortableTrackKey, Register<(PortableTrack, bool)>>,
) -> Vec<PortableTrack> {
    let mut values = radio
        .values()
        .filter(|register| register.value.1)
        .collect::<Vec<_>>();
    values.sort_by(|left, right| {
        right
            .stamp
            .dot
            .cmp(&left.stamp.dot)
            .then(left.value.0.key.cmp(&right.value.0.key))
    });
    values
        .into_iter()
        .take(RADIO_MAX)
        .map(|register| register.value.0.clone())
        .collect()
}

fn retained_events(
    events: impl IntoIterator<Item = EngagementEvent>,
    now_unix: i64,
) -> Vec<EngagementEvent> {
    let cutoff = now_unix.saturating_sub(RAW_EVENT_RETENTION_SECS);
    let mut events = events
        .into_iter()
        .filter(|event| {
            event.envelope.stamp.recorded_at_unix == 0
                || event.envelope.stamp.recorded_at_unix >= cutoff
        })
        .collect::<Vec<_>>();
    events.sort_by(|left, right| {
        right
            .envelope
            .stamp
            .recorded_at_unix
            .cmp(&left.envelope.stamp.recorded_at_unix)
            .then(right.envelope.stamp.dot.cmp(&left.envelope.stamp.dot))
            .then(left.event_id.cmp(&right.event_id))
    });
    events.truncate(ENGAGEMENT_EVENTS_MAX);
    events.reverse();
    events
}

fn apply_engagement_projection(legacy: &mut LegacyProjection, events: &[EngagementEvent]) {
    let mut history_order = legacy.history.clone();
    let mut known_play_events = legacy
        .signals
        .play_log
        .iter()
        .map(|event| event.event_id.clone())
        .collect::<HashSet<_>>();
    for event in events {
        let completion = completion_fraction(event.played_duration_ms, event.total_duration_ms);
        let signal = legacy
            .signals
            .tracks
            .entry(event.track.key.clone())
            .or_insert_with(|| LegacyTrackSignal {
                track: event.track.clone(),
                play_count: 0,
                completed_count: 0,
                skip_count: 0,
                last_played_at: 0,
                last_completion: 0.0,
                disliked: false,
            });
        signal.track = event.track.clone();
        signal.last_played_at = signal
            .last_played_at
            .max(event.envelope.stamp.recorded_at_unix);
        signal.last_completion = completion;
        match event.engagement {
            EngagementKind::QuickSkip => {
                signal.skip_count = signal.skip_count.saturating_add(1);
                bump_artist(
                    &mut legacy.signals.artist_affinity,
                    &event.artist_key,
                    skip_delta(completion),
                );
            }
            EngagementKind::Completion => {
                signal.completed_count = signal.completed_count.saturating_add(1);
                bump_artist(&mut legacy.signals.artist_affinity, &event.artist_key, 0.05);
            }
            EngagementKind::Play => {
                signal.play_count = signal.play_count.saturating_add(1);
            }
        }
        if event.engagement == EngagementKind::Play
            && known_play_events.insert(event.event_id.clone())
        {
            legacy.signals.play_log.push(LegacyPlayEvent {
                event_id: event.event_id.clone(),
                track: event.track.clone(),
                played_at: event.envelope.stamp.recorded_at_unix,
            });
        }
        if event.engagement == EngagementKind::Play {
            history_order.retain(|track| track.key != event.track.key);
            history_order.insert(0, event.track.clone());
        }
    }
    history_order.truncate(HISTORY_MAX);
    legacy.history = history_order;
}

fn apply_disliked_projection(
    signals: &mut LegacySignals,
    ratings: &BTreeMap<PortableTrackKey, Register<(PortableTrack, Rating)>>,
) {
    for signal in signals.tracks.values_mut() {
        signal.disliked = false;
    }
    for (key, register) in ratings {
        let signal = signals
            .tracks
            .entry(key.clone())
            .or_insert_with(|| LegacyTrackSignal {
                track: register.value.0.clone(),
                play_count: 0,
                completed_count: 0,
                skip_count: 0,
                last_played_at: register.stamp.recorded_at_unix,
                last_completion: 0.0,
                disliked: false,
            });
        signal.track = register.value.0.clone();
        signal.disliked = register.value.1 == Rating::Disliked;
    }
}

fn apply_rating_affinity_delta(
    signals: &mut LegacySignals,
    baseline: &BTreeMap<PortableTrackKey, (PortableTrack, Rating)>,
    final_ratings: &BTreeMap<PortableTrackKey, Register<(PortableTrack, Rating)>>,
) {
    let keys = baseline
        .keys()
        .chain(final_ratings.keys())
        .cloned()
        .collect::<BTreeSet<_>>();
    for key in keys {
        let before = baseline
            .get(&key)
            .map(|(_, rating)| *rating)
            .unwrap_or_default();
        let (track, after) = final_ratings.get(&key).map_or_else(
            || {
                baseline
                    .get(&key)
                    .map(|(track, _)| (track.clone(), Rating::Neutral))
                    .unwrap_or_else(|| {
                        (
                            PortableTrack {
                                key: key.clone(),
                                title: String::new(),
                                artist: String::new(),
                                album: None,
                                duration_secs: None,
                                isrc: None,
                            },
                            Rating::Neutral,
                        )
                    })
            },
            |register| (register.value.0.clone(), register.value.1),
        );
        let delta = rating_affinity(after) - rating_affinity(before);
        let artist_key = crate::signals::normalize_artist(&track.artist);
        bump_artist(&mut signals.artist_affinity, &artist_key, delta);
    }
}

fn rating_affinity(rating: Rating) -> f32 {
    match rating {
        Rating::Neutral => 0.0,
        Rating::Liked => 0.30,
        Rating::Disliked => -0.60,
    }
}

fn completion_fraction(played_ms: Option<u64>, total_ms: Option<u64>) -> f32 {
    match (played_ms, total_ms) {
        (Some(played), Some(total)) if total > 0 => {
            (played as f64 / total as f64).clamp(0.0, 1.0) as f32
        }
        _ => 0.0,
    }
}

fn skip_delta(completion: f32) -> f32 {
    if completion < 0.10 {
        -0.30
    } else if completion < 0.40 {
        -0.15
    } else if completion < 0.75 {
        -0.05
    } else {
        0.0
    }
}

fn bump_artist(weights: &mut BTreeMap<String, f32>, artist_key: &str, delta: f32) {
    if artist_key.is_empty() || delta == 0.0 {
        return;
    }
    let weight = weights.entry(artist_key.to_owned()).or_default();
    *weight = (*weight + delta).clamp(ARTIST_WEIGHT_MIN, ARTIST_WEIGHT_MAX);
}

fn empty_playlist(
    envelope: &OperationEnvelope,
    playlist_id: &PlaylistId,
    name: &str,
) -> PlaylistRegisters {
    PlaylistRegisters {
        slug: format!(
            "playlist-{}",
            super::legacy::stable_hash(playlist_id.as_str())
        ),
        name: Register::new(envelope, name.to_owned()),
        deleted: Register::new(envelope, false),
        entries: BTreeMap::new(),
    }
}

fn project_playlists(
    playlists: BTreeMap<PlaylistId, PlaylistRegisters>,
) -> (Vec<LegacyPlaylist>, usize) {
    let mut repaired = 0;
    let mut result = Vec::new();
    for (playlist_id, playlist) in playlists {
        if playlist.deleted.value {
            continue;
        }
        let mut live = playlist
            .entries
            .into_iter()
            .filter(|(_, entry)| !entry.removed.value)
            .collect::<BTreeMap<_, _>>();
        repaired += repair_cycles(&mut live);
        let ordered = order_entries(&live);
        result.push(LegacyPlaylist {
            playlist_id,
            slug: playlist.slug,
            name: playlist.name.value,
            entries: ordered
                .into_iter()
                .take(PLAYLIST_ENTRIES_MAX)
                .filter_map(|entry_id| {
                    live.remove(&entry_id).map(|entry| LegacyPlaylistEntry {
                        entry_id,
                        track: entry.track.value,
                    })
                })
                .collect(),
        });
    }
    result.sort_by(|left, right| left.playlist_id.cmp(&right.playlist_id));
    result.truncate(PLAYLISTS_MAX);
    (result, repaired)
}

fn repair_cycles(entries: &mut BTreeMap<PlaylistEntryId, EntryRegisters>) -> usize {
    let mut repaired = 0;
    while let Some(cycle) = find_cycle(entries) {
        let root = cycle
            .into_iter()
            .min_by(|left, right| {
                let left = &entries[left].position;
                let right = &entries[right].position;
                stamp_order(
                    &left.stamp,
                    &left.operation_id,
                    &right.stamp,
                    &right.operation_id,
                )
            })
            .expect("a detected cycle is non-empty");
        entries
            .get_mut(&root)
            .expect("cycle entry remains present")
            .position
            .value = None;
        repaired += 1;
    }
    repaired
}

fn find_cycle(entries: &BTreeMap<PlaylistEntryId, EntryRegisters>) -> Option<Vec<PlaylistEntryId>> {
    for start in entries.keys() {
        let mut positions = HashMap::<PlaylistEntryId, usize>::new();
        let mut path = Vec::new();
        let mut cursor = Some(start.clone());
        while let Some(entry_id) = cursor {
            let Some(entry) = entries.get(&entry_id) else {
                break;
            };
            if let Some(index) = positions.get(&entry_id).copied() {
                return Some(path[index..].to_vec());
            }
            positions.insert(entry_id.clone(), path.len());
            path.push(entry_id);
            cursor = entry.position.value.clone();
        }
    }
    None
}

fn order_entries(entries: &BTreeMap<PlaylistEntryId, EntryRegisters>) -> Vec<PlaylistEntryId> {
    let mut children = BTreeMap::<Option<PlaylistEntryId>, Vec<PlaylistEntryId>>::new();
    for (entry_id, entry) in entries {
        let parent = entry
            .position
            .value
            .as_ref()
            .filter(|parent| entries.contains_key(*parent))
            .cloned();
        children.entry(parent).or_default().push(entry_id.clone());
    }
    for siblings in children.values_mut() {
        siblings.sort();
    }
    let mut ordered = Vec::with_capacity(entries.len());
    let mut visited = HashSet::new();
    append_children(None, &children, &mut visited, &mut ordered);
    for entry_id in entries.keys() {
        if visited.insert(entry_id.clone()) {
            ordered.push(entry_id.clone());
            append_children(
                Some(entry_id.clone()),
                &children,
                &mut visited,
                &mut ordered,
            );
        }
    }
    ordered
}

fn append_children(
    parent: Option<PlaylistEntryId>,
    children: &BTreeMap<Option<PlaylistEntryId>, Vec<PlaylistEntryId>>,
    visited: &mut HashSet<PlaylistEntryId>,
    ordered: &mut Vec<PlaylistEntryId>,
) {
    let Some(siblings) = children.get(&parent) else {
        return;
    };
    for entry_id in siblings {
        if visited.insert(entry_id.clone()) {
            ordered.push(entry_id.clone());
            append_children(Some(entry_id.clone()), children, visited, ordered);
        }
    }
}

fn apply_track_bindings(
    ratings: &mut BTreeMap<PortableTrackKey, Register<(PortableTrack, Rating)>>,
    radio: &mut BTreeMap<PortableTrackKey, Register<(PortableTrack, bool)>>,
    bindings: &BTreeMap<PortableTrackKey, Register<PortableTrackKey>>,
) {
    for (placeholder, binding) in bindings {
        if let Some(mut register) = ratings.remove(placeholder) {
            register.value.0.key = binding.value.clone();
            ratings
                .entry(binding.value.clone())
                .and_modify(|current| {
                    if stamp_order(
                        &register.stamp,
                        &register.operation_id,
                        &current.stamp,
                        &current.operation_id,
                    )
                    .is_gt()
                    {
                        *current = register.clone();
                    }
                })
                .or_insert(register);
        }
        if let Some(mut register) = radio.remove(placeholder) {
            register.value.0.key = binding.value.clone();
            radio
                .entry(binding.value.clone())
                .and_modify(|current| {
                    if stamp_order(
                        &register.stamp,
                        &register.operation_id,
                        &current.stamp,
                        &current.operation_id,
                    )
                    .is_gt()
                    {
                        *current = register.clone();
                    }
                })
                .or_insert(register);
        }
    }
}

fn enforce_projection_caps(legacy: &mut LegacyProjection) {
    legacy.favorites.truncate(FAVORITES_MAX);
    legacy.history.truncate(HISTORY_MAX);
    legacy.radio_favorites.truncate(RADIO_MAX);
    legacy.radio_history.truncate(RADIO_MAX);
    legacy.playlists.truncate(PLAYLISTS_MAX);
    for playlist in &mut legacy.playlists {
        playlist.entries.truncate(PLAYLIST_ENTRIES_MAX);
    }
    legacy.signals.play_log.sort_by(|left, right| {
        left.played_at
            .cmp(&right.played_at)
            .then(left.event_id.cmp(&right.event_id))
    });
    if legacy.signals.play_log.len() > ENGAGEMENT_EVENTS_MAX {
        let remove = legacy.signals.play_log.len() - ENGAGEMENT_EVENTS_MAX;
        legacy.signals.play_log.drain(..remove);
    }
    if legacy.signals.tracks.len() > SIGNAL_TRACKS_MAX {
        let mut eviction = legacy
            .signals
            .tracks
            .iter()
            .map(|(key, signal)| (signal.disliked, signal.last_played_at, key.clone()))
            .collect::<Vec<_>>();
        eviction.sort_by(|left, right| {
            left.0
                .cmp(&right.0)
                .then(left.1.cmp(&right.1))
                .then(left.2.cmp(&right.2))
        });
        for (_, _, key) in eviction
            .into_iter()
            .take(legacy.signals.tracks.len() - SIGNAL_TRACKS_MAX)
        {
            legacy.signals.tracks.remove(&key);
        }
    }
}

fn projection_fingerprint(projection: &LegacyProjection) -> Result<String, PersonalStateError> {
    // Fingerprint the exact sanitized shape that can round-trip through the four legacy runtime
    // stores. The ledger intentionally retains more raw events and stable playlist IDs than those
    // projections, so hashing the richer in-memory form would report a false mismatch on restart.
    let (library, playlists, signals, station) = projection.clone().into_runtime();
    let runtime = LegacyProjection::from_runtime(&library, &playlists, &signals, &station);
    Ok(sha256_hex(&serde_json::to_vec(&runtime)?))
}

fn empty_legacy() -> LegacyProjection {
    LegacyProjection {
        favorites: Vec::new(),
        history: Vec::new(),
        radio_favorites: Vec::new(),
        radio_history: Vec::new(),
        playlists: Vec::new(),
        signals: LegacySignals::default(),
        station: LegacyStation::default(),
    }
}

fn merge_device_registry(target: &mut DeviceRegistry, source: &DeviceRegistry) {
    for (device_id, source_device) in source {
        target
            .entry(device_id.clone())
            .and_modify(|target_device| {
                if source_device.revoked {
                    target_device.revoked = true;
                } else if !target_device.revoked && source_device.name < target_device.name {
                    target_device.name = source_device.name.clone();
                }
            })
            .or_insert_with(|| source_device.clone());
    }
}

fn choose_checkpoint(
    left: Option<&super::CompactionCheckpoint>,
    right: Option<&super::CompactionCheckpoint>,
) -> Option<super::CompactionCheckpoint> {
    match (left, right) {
        (Some(left), Some(right)) => Some(
            if checkpoint_order(left, right).is_ge() {
                left
            } else {
                right
            }
            .clone(),
        ),
        (Some(checkpoint), None) | (None, Some(checkpoint)) => Some(checkpoint.clone()),
        (None, None) => None,
    }
}

fn checkpoint_order(
    left: &super::CompactionCheckpoint,
    right: &super::CompactionCheckpoint,
) -> Ordering {
    let left_coverage = left.coverage.0.values().copied().sum::<u64>();
    let right_coverage = right.coverage.0.values().copied().sum::<u64>();
    left_coverage
        .cmp(&right_coverage)
        .then(left.checkpoint_id.cmp(&right.checkpoint_id))
}
