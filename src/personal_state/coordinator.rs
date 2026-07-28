use std::collections::{BTreeMap, BTreeSet, HashSet};

use serde::{Deserialize, Serialize};

use super::legacy::{
    LegacyPlaylist, LegacyPlaylistEntry, LegacyProjection, rating_from_legacy, sha256_hex,
    stable_hash,
};
use super::{
    CausalStamp, DeviceId, Dot, EngagementKind, Operation, OperationEnvelope, OperationOrigin,
    PersonalStateError, PersonalStateV2, PlaylistEntryId, PlaylistId, PortableTrack,
    PortableTrackKey, project, refresh_device_registry,
};

const MAX_EXTERNAL_OPERATION_BATCH: usize = 4_096;

/// One operation in an external bridge batch.
///
/// The acknowledgement ID is portable bridge state. The ledger wraps it in a deterministic
/// device-scoped envelope so two devices may observe and merge the same server change safely.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalOperationInput {
    pub acknowledgement_id: String,
    pub operation: Operation,
    pub recorded_at_unix: i64,
}

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
    append_operation_with_origin_as(
        state,
        local_device_id,
        None,
        OperationOrigin::Local,
        operation,
        recorded_at_unix,
    )
}

/// Append one operation observed from an external bridge under the local device's causal dot.
///
/// `operation_id` is the bridge's portable acknowledgement key. The ledger envelope derives a
/// deterministic device-scoped identifier from it, while a `RecordEngagement` keeps the portable
/// key as its event ID. Replaying the same observation on one device is therefore a no-op, while
/// two devices may independently observe and safely merge the same portable event.
pub fn append_external_operation_as(
    state: &PersonalStateV2,
    local_device_id: &DeviceId,
    operation_id: String,
    origin: OperationOrigin,
    operation: Operation,
    recorded_at_unix: i64,
) -> Result<PersonalStateV2, PersonalStateError> {
    if matches!(origin, OperationOrigin::Local) {
        return Err(PersonalStateError::InvalidOperation(
            "external operation origin cannot be local",
        ));
    }
    validate_external_origin(&origin, &operation)?;
    let envelope_id = external_operation_envelope_id(local_device_id, &operation_id)?;
    append_operation_with_origin_as(
        state,
        local_device_id,
        Some(envelope_id),
        origin,
        operation,
        recorded_at_unix,
    )
}

/// Append one externally observed operation for an unsynced single-device ledger.
///
/// Before encrypted sync is configured, the deterministic migration device deliberately has no
/// signing identity. That device may still own local-first server observations; once a real sync
/// device is enrolled, callers must switch to [`append_external_operation_as`] so the author is
/// explicit.
pub fn append_external_operation(
    state: &PersonalStateV2,
    operation_id: String,
    origin: OperationOrigin,
    operation: Operation,
    recorded_at_unix: i64,
) -> Result<PersonalStateV2, PersonalStateError> {
    if matches!(origin, OperationOrigin::Local) {
        return Err(PersonalStateError::InvalidOperation(
            "external operation origin cannot be local",
        ));
    }
    validate_external_origin(&origin, &operation)?;
    let local_device_id = local_device(state)?;
    let envelope_id = external_operation_envelope_id(&local_device_id, &operation_id)?;
    append_operation_with_origin_as_inner(
        state,
        &local_device_id,
        Some(envelope_id),
        origin,
        operation,
        recorded_at_unix,
        false,
    )
}

/// Append one external batch under an explicitly enrolled local device.
///
/// The candidate is built and validated in memory before it is returned, so callers can persist
/// every playlist entry change in one Personal State transaction. Replaying a completely or
/// partially present batch is an idempotent no-op; conflicting reuse of any acknowledgement ID
/// rejects the whole candidate.
pub fn append_external_operations_as(
    state: &PersonalStateV2,
    local_device_id: &DeviceId,
    origin: OperationOrigin,
    operations: &[ExternalOperationInput],
) -> Result<(PersonalStateV2, Vec<String>), PersonalStateError> {
    append_external_operations_inner(state, local_device_id, origin, operations, true)
}

/// Append one external batch for the deterministic unsynced single-device ledger.
pub fn append_external_operations(
    state: &PersonalStateV2,
    origin: OperationOrigin,
    operations: &[ExternalOperationInput],
) -> Result<(PersonalStateV2, Vec<String>), PersonalStateError> {
    let local_device_id = local_device(state)?;
    append_external_operations_inner(state, &local_device_id, origin, operations, false)
}

/// Return the stable ledger envelope ID for one external acknowledgement on one local device.
///
/// The bridge acknowledgement remains portable across devices. Only the surrounding ledger
/// operation is scoped so independently authored observations cannot collide during merge.
pub fn external_operation_envelope_id(
    local_device_id: &DeviceId,
    acknowledgement_id: &str,
) -> Result<String, PersonalStateError> {
    super::model::validate_id(
        "external operation acknowledgement id",
        acknowledgement_id,
        256,
    )?;
    let mut material =
        Vec::with_capacity(48 + local_device_id.as_str().len() + acknowledgement_id.len());
    material.extend_from_slice(b"yututui-external-operation-envelope-v1\0");
    for part in [
        local_device_id.as_str().as_bytes(),
        acknowledgement_id.as_bytes(),
    ] {
        material.extend_from_slice(&(part.len() as u64).to_be_bytes());
        material.extend_from_slice(part);
    }
    Ok(format!("external-{}", sha256_hex(&material)))
}

fn validate_external_origin(
    origin: &OperationOrigin,
    operation: &Operation,
) -> Result<(), PersonalStateError> {
    let OperationOrigin::OpenSubsonic { backend_id } = origin else {
        return Ok(());
    };
    match operation {
        Operation::SetRating { track, .. }
        | Operation::RecordEngagement { track, .. }
        | Operation::UpsertPlaylistEntry { track, .. } => match &track.key {
            PortableTrackKey::OpenSubsonic {
                backend_id: track_backend,
                ..
            } if track_backend == backend_id => Ok(()),
            _ => Err(PersonalStateError::InvalidOperation(
                "OpenSubsonic origin does not match track backend",
            )),
        },
        Operation::UpsertPlaylist { .. }
        | Operation::DeletePlaylist { .. }
        | Operation::MovePlaylistEntry { .. }
        | Operation::RemovePlaylistEntry { .. } => Ok(()),
        _ => Err(PersonalStateError::InvalidOperation(
            "OpenSubsonic origin requires a rating, engagement, or playlist observation",
        )),
    }
}

fn append_external_operations_inner(
    state: &PersonalStateV2,
    local_device_id: &DeviceId,
    origin: OperationOrigin,
    operations: &[ExternalOperationInput],
    require_keyed: bool,
) -> Result<(PersonalStateV2, Vec<String>), PersonalStateError> {
    if matches!(origin, OperationOrigin::Local) {
        return Err(PersonalStateError::InvalidOperation(
            "external operation origin cannot be local",
        ));
    }
    if operations.is_empty() || operations.len() > MAX_EXTERNAL_OPERATION_BATCH {
        return Err(PersonalStateError::InvalidOperation(
            "external operation batch size is invalid",
        ));
    }
    state.validate()?;
    validate_local_device_binding(state, local_device_id, require_keyed)?;

    let mut acknowledgement_ids = HashSet::with_capacity(operations.len());
    let mut envelope_ids = Vec::with_capacity(operations.len());
    for input in operations {
        if !acknowledgement_ids.insert(input.acknowledgement_id.as_str()) {
            return Err(PersonalStateError::ConflictingOperationId);
        }
        validate_external_origin(&origin, &input.operation)?;
        let envelope_id =
            external_operation_envelope_id(local_device_id, &input.acknowledgement_id)?;
        if let Some(existing) = state
            .operations
            .iter()
            .find(|existing| existing.operation_id == envelope_id)
            && (existing.origin != origin || existing.operation != input.operation)
        {
            return Err(PersonalStateError::ConflictingOperationId);
        }
        envelope_ids.push(envelope_id);
    }

    let mut candidate = state.clone();
    let mut appender = OperationAppender::new(&mut candidate, local_device_id.clone());
    for (input, envelope_id) in operations.iter().zip(&envelope_ids) {
        if appender
            .state
            .operations
            .iter()
            .any(|existing| existing.operation_id == *envelope_id)
        {
            continue;
        }
        appender.append_with_metadata(
            Some(envelope_id.clone()),
            origin.clone(),
            input.operation.clone(),
            input.recorded_at_unix,
        )?;
    }
    refresh_device_registry(&mut candidate)?;
    candidate.normalize()?;
    Ok((candidate, envelope_ids))
}

fn append_operation_with_origin_as(
    state: &PersonalStateV2,
    local_device_id: &DeviceId,
    operation_id: Option<String>,
    origin: OperationOrigin,
    operation: Operation,
    recorded_at_unix: i64,
) -> Result<PersonalStateV2, PersonalStateError> {
    append_operation_with_origin_as_inner(
        state,
        local_device_id,
        operation_id,
        origin,
        operation,
        recorded_at_unix,
        true,
    )
}

fn append_operation_with_origin_as_inner(
    state: &PersonalStateV2,
    local_device_id: &DeviceId,
    operation_id: Option<String>,
    origin: OperationOrigin,
    operation: Operation,
    recorded_at_unix: i64,
    require_keyed: bool,
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
    validate_local_device_binding(state, local_device_id, require_keyed && !enrollment)?;

    if let Some(operation_id) = operation_id.as_deref()
        && let Some(existing) = state
            .operations
            .iter()
            .find(|existing| existing.operation_id == operation_id)
    {
        return if existing.origin == origin && existing.operation == operation {
            Ok(state.clone())
        } else {
            Err(PersonalStateError::ConflictingOperationId)
        };
    }

    let mut candidate = state.clone();
    OperationAppender::new(&mut candidate, local_device_id.clone()).append_with_metadata(
        operation_id,
        origin,
        operation,
        recorded_at_unix,
    )?;
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
        self.append_with_metadata(None, OperationOrigin::Local, operation, recorded_at_unix)
    }

    fn append_with_metadata(
        &mut self,
        operation_id: Option<String>,
        origin: OperationOrigin,
        operation: Operation,
        recorded_at_unix: i64,
    ) -> Result<Dot, PersonalStateError> {
        // A different durable state must never reuse the terminal revision. The transaction
        // coordinator advances this revision when it publishes the candidate; rejecting here
        // keeps a MAX-valued imported ledger immutable instead of manufacturing same-revision
        // content which a detached sync worker could mistake for its original snapshot.
        let _ = self.state.next_revision()?;
        let sequence = self.next_sequence()?;
        let dot = Dot {
            device_id: self.device.clone(),
            sequence,
        };
        let envelope = OperationEnvelope {
            operation_id: operation_id
                .unwrap_or_else(|| format!("{}:{sequence}", self.device.as_str())),
            stamp: CausalStamp {
                dot: dot.clone(),
                observed: self.state.version_vector.clone(),
                recorded_at_unix,
            },
            origin,
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
    let base_ratings = rating_from_legacy(&base.favorites, &base.signals);
    let current_ratings = rating_from_legacy(&current.favorites, &current.signals);
    let keys = base_ratings
        .keys()
        .chain(current_ratings.keys())
        .cloned()
        .collect::<BTreeSet<_>>();
    for key in keys {
        let before = base_ratings
            .get(&key)
            .map(|(_, rating)| *rating)
            .unwrap_or_default();
        let after = current_ratings
            .get(&key)
            .map(|(_, rating)| *rating)
            .unwrap_or_default();
        if before == after {
            continue;
        }
        let track = match (current_ratings.get(&key), base_ratings.get(&key)) {
            (Some((current, _)), base_rating) => {
                let existing = base_rating
                    .map(|(track, _)| track)
                    .or_else(|| base.signals.tracks.get(&key).map(|signal| &signal.track));
                existing.map_or_else(
                    || current.clone(),
                    |existing| enrich_portable_track(current.clone(), existing),
                )
            }
            (None, Some((base, _))) => base.clone(),
            (None, None) => unreachable!("union key has a track"),
        };
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

/// Preserve portable metadata that the legacy signal store cannot represent.
///
/// A disliked track may no longer occur in favorites, history, or playlists. In that case the
/// runtime-to-v2 adapter can recover its exact key from `Signals`, but only has empty fallback
/// metadata. Reconciliation must not let that lossy projection erase metadata already carried by
/// the winning v2 rating operation, since the artist is also the stable input to affinity
/// projection.
fn enrich_portable_track(mut current: PortableTrack, existing: &PortableTrack) -> PortableTrack {
    debug_assert_eq!(current.key, existing.key);
    if current.title.is_empty() {
        current.title.clone_from(&existing.title);
    }
    if current.artist.is_empty() {
        current.artist.clone_from(&existing.artist);
    }
    if current.album.is_none() {
        current.album.clone_from(&existing.album);
    }
    if current.duration_secs.is_none() {
        current.duration_secs = existing.duration_secs;
    }
    if current.isrc.is_none() {
        current.isrc.clone_from(&existing.isrc);
    }
    current
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
