use std::collections::{BTreeMap, BTreeSet, HashSet};

use super::legacy::{
    LegacyPlaylist, LegacyPlaylistEntry, LegacyProjection, rating_from_legacy, stable_hash,
};
use super::{
    CausalStamp, DeviceId, Dot, EngagementKind, Operation, OperationEnvelope, OperationOrigin,
    PersonalStateError, PersonalStateV2, PlaylistEntryId, PlaylistId, PortableTrack,
    PortableTrackKey, project, refresh_device_registry,
};

/// Convert mutations visible in the four runtime projections into causal v2 operations.
pub(crate) fn reconcile_runtime(
    state: &PersonalStateV2,
    library: &crate::library::Library,
    playlists: &crate::playlists::Playlists,
    signals: &crate::signals::Signals,
    station: &crate::station::StationStore,
) -> Result<PersonalStateV2, PersonalStateError> {
    let device = local_device(state)?;
    reconcile_runtime_for_device(state, device, library, playlists, signals, station, false)
}

/// Reconcile runtime mutations using the device explicitly bound to this process.
///
/// Synced ledgers must never guess which active device owns a new causal dot.
pub fn reconcile_runtime_as(
    state: &PersonalStateV2,
    local_device_id: &DeviceId,
    library: &crate::library::Library,
    playlists: &crate::playlists::Playlists,
    signals: &crate::signals::Signals,
    station: &crate::station::StationStore,
) -> Result<PersonalStateV2, PersonalStateError> {
    reconcile_runtime_for_device(
        state,
        local_device_id.clone(),
        library,
        playlists,
        signals,
        station,
        true,
    )
}

/// Append one local operation under an explicit device binding.
pub fn append_operation_as(
    state: &PersonalStateV2,
    local_device_id: &DeviceId,
    operation: Operation,
    recorded_at_unix: i64,
) -> Result<PersonalStateV2, PersonalStateError> {
    state.validate()?;
    let enrollment = matches!(
        &operation,
        Operation::AddDevice { device }
            if &device.device_id == local_device_id
                && device.public_identity.is_some()
                && state
                    .device_registry
                    .get(local_device_id)
                    .is_some_and(|current| current.public_identity.is_none())
    );
    validate_local_device_binding(state, local_device_id, !enrollment)?;

    let mut candidate = state.clone();
    OperationAppender::new(&mut candidate, local_device_id.clone())
        .append(operation, recorded_at_unix)?;
    refresh_device_registry(&mut candidate)?;
    candidate.normalize()?;
    Ok(candidate)
}

fn reconcile_runtime_for_device(
    state: &PersonalStateV2,
    device: DeviceId,
    library: &crate::library::Library,
    playlists: &crate::playlists::Playlists,
    signals: &crate::signals::Signals,
    station: &crate::station::StationStore,
    require_keyed_binding: bool,
) -> Result<PersonalStateV2, PersonalStateError> {
    state.validate()?;
    validate_local_device_binding(state, &device, require_keyed_binding)?;
    let mut candidate = state.clone();
    let base = project(state)?.legacy;
    let current = LegacyProjection::from_runtime(library, playlists, signals, station);
    let mut appender = OperationAppender::new(&mut candidate, device);

    reconcile_ratings(&base, &current, &mut appender)?;
    reconcile_radio(&base, &current, &mut appender)?;
    reconcile_engagement(&base, &current, &mut appender)?;
    reconcile_playlists(&base.playlists, &current.playlists, &mut appender)?;
    reconcile_station(&base, &current, &mut appender)?;

    candidate.normalize()?;
    Ok(candidate)
}

struct OperationAppender<'a> {
    state: &'a mut PersonalStateV2,
    device: DeviceId,
}

impl<'a> OperationAppender<'a> {
    fn new(state: &'a mut PersonalStateV2, device: DeviceId) -> Self {
        Self { state, device }
    }

    fn next_sequence(&self) -> Result<u64, PersonalStateError> {
        self.state
            .version_vector
            .observed(&self.device)
            .checked_add(1)
            .ok_or(PersonalStateError::InvalidOperation(
                "local operation sequence exhausted",
            ))
    }

    fn append(
        &mut self,
        operation: Operation,
        recorded_at_unix: i64,
    ) -> Result<Dot, PersonalStateError> {
        let sequence = self.next_sequence()?;
        let dot = Dot {
            device_id: self.device.clone(),
            sequence,
        };
        let envelope = OperationEnvelope {
            operation_id: format!("{}:{sequence}", self.device.as_str()),
            stamp: CausalStamp {
                dot: dot.clone(),
                observed: self.state.version_vector.clone(),
                recorded_at_unix,
            },
            origin: OperationOrigin::Local,
            operation,
        };
        self.state.version_vector.observe(&dot);
        self.state.operations.push(envelope);
        self.state.projection_fingerprint = None;
        Ok(dot)
    }

    fn event(
        &mut self,
        track: PortableTrack,
        engagement: EngagementKind,
        completion: f32,
        recorded_at_unix: i64,
    ) -> Result<(), PersonalStateError> {
        let total_duration_ms = track
            .duration_secs
            .map(|seconds| u64::from(seconds).saturating_mul(1_000));
        let played_duration_ms = total_duration_ms
            .map(|total| (total as f64 * f64::from(completion.clamp(0.0, 1.0))).round() as u64);
        let sequence = self.next_sequence()?;
        let event_id = format!("event-{}-{sequence}", self.device.as_str());
        let artist_key = crate::signals::normalize_artist(&track.artist);
        self.append(
            Operation::RecordEngagement {
                event_id,
                track,
                engagement,
                played_duration_ms,
                total_duration_ms,
                artist_key,
            },
            recorded_at_unix,
        )?;
        Ok(())
    }
}

fn local_device(state: &PersonalStateV2) -> Result<DeviceId, PersonalStateError> {
    state.validate()?;
    let mut active = state
        .device_registry
        .values()
        .filter(|device| !device.revoked && device.device_id.as_str() != "legacy")
        .map(|device| device.device_id.clone());
    let device = active.next().ok_or(PersonalStateError::InvalidOperation(
        "personal state has no active local device",
    ))?;
    if active.next().is_some() {
        return Err(PersonalStateError::InvalidOperation(
            "multiple active devices require an explicit local device binding",
        ));
    }
    Ok(device)
}

fn validate_local_device_binding(
    state: &PersonalStateV2,
    local_device_id: &DeviceId,
    require_keyed: bool,
) -> Result<(), PersonalStateError> {
    let device =
        state
            .device_registry
            .get(local_device_id)
            .ok_or(PersonalStateError::InvalidOperation(
                "local device binding is not in the registry",
            ))?;
    if device.revoked {
        return Err(PersonalStateError::InvalidOperation(
            "local device binding is revoked",
        ));
    }
    if device.device_id.as_str() == "legacy" {
        return Err(PersonalStateError::InvalidOperation(
            "legacy migration device cannot own local operations",
        ));
    }
    if require_keyed && device.public_identity.is_none() {
        return Err(PersonalStateError::InvalidOperation(
            "local device binding has no public identity",
        ));
    }
    Ok(())
}

fn reconcile_ratings(
    base: &LegacyProjection,
    current: &LegacyProjection,
    appender: &mut OperationAppender<'_>,
) -> Result<(), PersonalStateError> {
    let base = rating_from_legacy(&base.favorites, &base.signals);
    let current = rating_from_legacy(&current.favorites, &current.signals);
    let keys = base
        .keys()
        .chain(current.keys())
        .cloned()
        .collect::<BTreeSet<_>>();
    for key in keys {
        let before = base
            .get(&key)
            .map(|(_, rating)| *rating)
            .unwrap_or_default();
        let after = current
            .get(&key)
            .map(|(_, rating)| *rating)
            .unwrap_or_default();
        if before == after {
            continue;
        }
        let track = current
            .get(&key)
            .or_else(|| base.get(&key))
            .map(|(track, _)| track.clone())
            .expect("union key has a track");
        appender.append(
            Operation::SetRating {
                track,
                rating: after,
            },
            crate::signals::unix_now(),
        )?;
    }
    Ok(())
}

fn reconcile_radio(
    base: &LegacyProjection,
    current: &LegacyProjection,
    appender: &mut OperationAppender<'_>,
) -> Result<(), PersonalStateError> {
    let base = base
        .radio_favorites
        .iter()
        .map(|track| (track.key.clone(), track))
        .collect::<BTreeMap<_, _>>();
    let current = current
        .radio_favorites
        .iter()
        .map(|track| (track.key.clone(), track))
        .collect::<BTreeMap<_, _>>();
    let keys = base
        .keys()
        .chain(current.keys())
        .cloned()
        .collect::<BTreeSet<_>>();
    for key in keys {
        let before = base.contains_key(&key);
        let after = current.contains_key(&key);
        if before == after {
            continue;
        }
        let station = (*current
            .get(&key)
            .or_else(|| base.get(&key))
            .expect("union key has a station"))
        .clone();
        appender.append(
            Operation::SetRadioFavorite {
                station,
                favorite: after,
            },
            crate::signals::unix_now(),
        )?;
    }
    Ok(())
}

fn reconcile_engagement(
    base: &LegacyProjection,
    current: &LegacyProjection,
    appender: &mut OperationAppender<'_>,
) -> Result<(), PersonalStateError> {
    let keys = base
        .signals
        .tracks
        .keys()
        .chain(current.signals.tracks.keys())
        .cloned()
        .collect::<BTreeSet<_>>();
    let mut emitted_plays = HashSet::<PortableTrackKey>::new();
    for key in keys {
        let before = base.signals.tracks.get(&key);
        let Some(after) = current.signals.tracks.get(&key) else {
            continue;
        };
        let before_play = before.map_or(0, |signal| signal.play_count);
        let before_completion = before.map_or(0, |signal| signal.completed_count);
        let before_skip = before.map_or(0, |signal| signal.skip_count);
        let recorded_at = if after.last_played_at == 0 {
            crate::signals::unix_now()
        } else {
            after.last_played_at
        };

        for _ in 0..after.play_count.saturating_sub(before_play) {
            appender.event(after.track.clone(), EngagementKind::Play, 0.0, recorded_at)?;
            emitted_plays.insert(key.clone());
        }
        for _ in 0..after.completed_count.saturating_sub(before_completion) {
            appender.event(
                after.track.clone(),
                EngagementKind::Completion,
                after.last_completion.max(0.90),
                recorded_at,
            )?;
        }
        for _ in 0..after.skip_count.saturating_sub(before_skip) {
            appender.event(
                after.track.clone(),
                EngagementKind::QuickSkip,
                after.last_completion,
                recorded_at,
            )?;
        }
    }

    if current.history.first().map(|track| &track.key)
        != base.history.first().map(|track| &track.key)
        && let Some(track) = current.history.first()
        && !emitted_plays.contains(&track.key)
    {
        appender.event(
            track.clone(),
            EngagementKind::Play,
            0.0,
            crate::signals::unix_now(),
        )?;
    }
    Ok(())
}

fn reconcile_playlists(
    base: &[LegacyPlaylist],
    current: &[LegacyPlaylist],
    appender: &mut OperationAppender<'_>,
) -> Result<(), PersonalStateError> {
    let base_by_slug = base
        .iter()
        .map(|playlist| (playlist.slug.clone(), playlist))
        .collect::<BTreeMap<_, _>>();
    let current_by_slug = current
        .iter()
        .map(|playlist| (playlist.slug.clone(), playlist))
        .collect::<BTreeMap<_, _>>();

    for (slug, playlist) in &base_by_slug {
        if !current_by_slug.contains_key(slug) {
            appender.append(
                Operation::DeletePlaylist {
                    playlist_id: playlist.playlist_id.clone(),
                    deleted: true,
                },
                crate::signals::unix_now(),
            )?;
        }
    }

    for (slug, current_playlist) in current_by_slug {
        let base_playlist = base_by_slug.get(&slug).copied();
        let playlist_id = base_playlist
            .map(|playlist| playlist.playlist_id.clone())
            .unwrap_or_else(|| current_playlist.playlist_id.clone());
        if base_playlist.is_none_or(|playlist| playlist.name != current_playlist.name) {
            appender.append(
                Operation::UpsertPlaylist {
                    playlist_id: playlist_id.clone(),
                    name: current_playlist.name.clone(),
                },
                crate::signals::unix_now(),
            )?;
        }
        reconcile_playlist_entries(
            &playlist_id,
            base_playlist
                .map(|playlist| playlist.entries.as_slice())
                .unwrap_or(&[]),
            &current_playlist.entries,
            appender,
        )?;
    }
    Ok(())
}

fn reconcile_playlist_entries(
    playlist_id: &PlaylistId,
    base: &[LegacyPlaylistEntry],
    current: &[LegacyPlaylistEntry],
    appender: &mut OperationAppender<'_>,
) -> Result<(), PersonalStateError> {
    let mut used = HashSet::<PlaylistEntryId>::new();
    let mut resolved = Vec::<(PlaylistEntryId, &PortableTrack)>::new();
    for (index, current_entry) in current.iter().enumerate() {
        let entry_id = base
            .iter()
            .find(|entry| {
                entry.track.key == current_entry.track.key && !used.contains(&entry.entry_id)
            })
            .map(|entry| entry.entry_id.clone())
            .unwrap_or_else(|| {
                PlaylistEntryId(format!(
                    "entry-{}",
                    stable_hash(&format!(
                        "{}\u{0}{index}\u{0}{:?}",
                        playlist_id.as_str(),
                        current_entry.track.key
                    ))
                ))
            });
        used.insert(entry_id.clone());
        resolved.push((entry_id, &current_entry.track));
    }

    for entry in base {
        if !used.contains(&entry.entry_id) {
            appender.append(
                Operation::RemovePlaylistEntry {
                    playlist_id: playlist_id.clone(),
                    entry_id: entry.entry_id.clone(),
                    removed: true,
                },
                crate::signals::unix_now(),
            )?;
        }
    }

    let base_positions = base
        .iter()
        .enumerate()
        .map(|(index, entry)| {
            (
                entry.entry_id.clone(),
                index
                    .checked_sub(1)
                    .map(|previous| base[previous].entry_id.clone()),
            )
        })
        .collect::<BTreeMap<_, _>>();
    let base_ids = base
        .iter()
        .map(|entry| entry.entry_id.clone())
        .collect::<HashSet<_>>();
    let mut previous = None;
    for (entry_id, track) in resolved {
        if !base_ids.contains(&entry_id) {
            appender.append(
                Operation::UpsertPlaylistEntry {
                    playlist_id: playlist_id.clone(),
                    entry_id: entry_id.clone(),
                    track: track.clone(),
                    after_entry_id: previous.clone(),
                },
                crate::signals::unix_now(),
            )?;
        } else if base_positions.get(&entry_id) != Some(&previous) {
            appender.append(
                Operation::MovePlaylistEntry {
                    playlist_id: playlist_id.clone(),
                    entry_id: entry_id.clone(),
                    after_entry_id: previous.clone(),
                },
                crate::signals::unix_now(),
            )?;
        }
        previous = Some(entry_id);
    }
    Ok(())
}

fn reconcile_station(
    base: &LegacyProjection,
    current: &LegacyProjection,
    appender: &mut OperationAppender<'_>,
) -> Result<(), PersonalStateError> {
    if base.station.query != current.station.query
        || base.station.explore != current.station.explore
    {
        appender.append(
            Operation::SetStationProfile {
                query: current.station.query.clone(),
                explore: current.station.explore,
            },
            crate::signals::unix_now(),
        )?;
    }
    let base_avoid = base
        .station
        .avoid_artist_keys
        .iter()
        .cloned()
        .collect::<BTreeSet<_>>();
    let current_avoid = current
        .station
        .avoid_artist_keys
        .iter()
        .cloned()
        .collect::<BTreeSet<_>>();
    for artist_key in base_avoid.union(&current_avoid) {
        let before = base_avoid.contains(artist_key);
        let after = current_avoid.contains(artist_key);
        if before != after {
            appender.append(
                Operation::SetAvoidArtist {
                    artist_key: artist_key.clone(),
                    avoid: after,
                },
                crate::signals::unix_now(),
            )?;
        }
    }
    Ok(())
}
