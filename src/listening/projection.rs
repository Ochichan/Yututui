use std::collections::BTreeMap;

use crate::personal_state::{
    CausalStamp, Operation, OperationEnvelope, PersonalStateError, PersonalStateV2,
    PortableTrackKey,
};

use super::{
    BookmarkId, BookmarkRecord, DjPreset, DjPresetId, ListeningOperation, PassportNote,
    PassportVisit, ResumeCandidate, ResumePoint,
};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ListeningProjection {
    pub bookmarks: BTreeMap<BookmarkId, Vec<BookmarkRecord>>,
    pub resumes: BTreeMap<PortableTrackKey, ResumeState>,
    pub passport_visits: BTreeMap<String, PassportVisit>,
    pub passport_notes: BTreeMap<String, Vec<PassportNote>>,
    pub dj_presets: BTreeMap<DjPresetId, Vec<DjPreset>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResumeState {
    pub track_key: PortableTrackKey,
    pub candidates: Vec<ResumeCandidate>,
}

impl ResumeState {
    pub fn automatic_point(&self) -> Option<&ResumePoint> {
        match self.candidates.as_slice() {
            [ResumeCandidate::Position(point)] => Some(point),
            _ => None,
        }
    }

    pub fn is_conflicted(&self) -> bool {
        self.candidates.len() > 1
    }
}

impl ListeningProjection {
    pub fn from_ledger(state: &PersonalStateV2) -> Result<Self, PersonalStateError> {
        state.validate()?;
        let mut bookmarks = BTreeMap::<BookmarkId, Vec<CrudRevision<BookmarkRecord>>>::new();
        let mut resumes = BTreeMap::<PortableTrackKey, Vec<Revision<ResumeCandidate>>>::new();
        let mut visits = BTreeMap::<String, Vec<CrudRevision<PassportVisit>>>::new();
        let mut notes = BTreeMap::<String, Vec<CrudRevision<PassportNote>>>::new();
        let mut passport_clears = Vec::<Revision<()>>::new();
        let mut presets = BTreeMap::<DjPresetId, Vec<CrudRevision<DjPreset>>>::new();

        for envelope in &state.operations {
            let Operation::Listening { change } = &envelope.operation else {
                continue;
            };
            change.validate()?;
            match change {
                ListeningOperation::UpsertBookmark { bookmark } => insert_crud(
                    bookmarks.entry(bookmark.bookmark_id.clone()).or_default(),
                    Revision::from_envelope(envelope, CrudValue::Present(bookmark.clone())),
                ),
                ListeningOperation::DeleteBookmark { bookmark_id } => insert_crud(
                    bookmarks.entry(bookmark_id.clone()).or_default(),
                    Revision::from_envelope(envelope, CrudValue::Deleted),
                ),
                ListeningOperation::SetResume { point } => {
                    validate_resume_device(envelope, &point.provenance.device_id)?;
                    insert_revision(
                        resumes.entry(point.track.key.clone()).or_default(),
                        Revision::from_envelope(envelope, ResumeCandidate::Position(point.clone())),
                    );
                }
                ListeningOperation::ClearResume { clear } => {
                    validate_resume_device(envelope, &clear.provenance.device_id)?;
                    insert_revision(
                        resumes.entry(clear.track.key.clone()).or_default(),
                        Revision::from_envelope(envelope, ResumeCandidate::Clear(clear.clone())),
                    );
                }
                ListeningOperation::RecordPassportVisit { visit } => push_revision(
                    visits.entry(visit.station_uuid.clone()).or_default(),
                    Revision::from_envelope(envelope, CrudValue::Present(visit.clone())),
                ),
                ListeningOperation::DeletePassportVisit { station_uuid } => push_revision(
                    visits.entry(station_uuid.clone()).or_default(),
                    Revision::from_envelope(envelope, CrudValue::Deleted),
                ),
                ListeningOperation::SetPassportNote { note } => insert_crud(
                    notes.entry(note.station_uuid.clone()).or_default(),
                    Revision::from_envelope(envelope, CrudValue::Present(note.clone())),
                ),
                ListeningOperation::DeletePassportNote { station_uuid } => insert_crud(
                    notes.entry(station_uuid.clone()).or_default(),
                    Revision::from_envelope(envelope, CrudValue::Deleted),
                ),
                ListeningOperation::ClearPassport => {
                    insert_revision(&mut passport_clears, Revision::from_envelope(envelope, ()))
                }
                ListeningOperation::UpsertDjPreset { preset } => insert_crud(
                    presets.entry(preset.preset_id.clone()).or_default(),
                    Revision::from_envelope(envelope, CrudValue::Present(preset.clone())),
                ),
                ListeningOperation::DeleteDjPreset { preset_id } => insert_crud(
                    presets.entry(preset_id.clone()).or_default(),
                    Revision::from_envelope(envelope, CrudValue::Deleted),
                ),
            }
        }

        apply_global_visit_deletes(&mut visits, &passport_clears);
        apply_global_deletes(&mut notes, &passport_clears);

        Ok(Self {
            bookmarks: visible_maps(bookmarks),
            resumes: resumes
                .into_iter()
                .filter_map(|(track_key, mut revisions)| {
                    sort_revisions(&mut revisions);
                    let candidates: Vec<_> = revisions.into_iter().map(|row| row.value).collect();
                    (!candidates.is_empty()).then_some((
                        track_key.clone(),
                        ResumeState {
                            track_key,
                            candidates,
                        },
                    ))
                })
                .collect(),
            passport_visits: visits
                .into_iter()
                .filter_map(|(key, revisions)| merge_visits(revisions).map(|visit| (key, visit)))
                .collect(),
            passport_notes: visible_maps(notes),
            dj_presets: visible_maps(presets),
        })
    }

    pub fn automatic_resume(&self, track_key: &PortableTrackKey) -> Option<&ResumePoint> {
        self.resumes.get(track_key)?.automatic_point()
    }
}

type CrudRevision<T> = Revision<CrudValue<T>>;

#[derive(Clone)]
struct Revision<T> {
    stamp: CausalStamp,
    operation_id: String,
    value: T,
}

impl<T> Revision<T> {
    fn from_envelope(envelope: &OperationEnvelope, value: T) -> Self {
        Self {
            stamp: envelope.stamp.clone(),
            operation_id: envelope.operation_id.clone(),
            value,
        }
    }
}

#[derive(Clone)]
enum CrudValue<T> {
    Present(T),
    Deleted,
}

fn insert_revision<T>(current: &mut Vec<Revision<T>>, candidate: Revision<T>) {
    if current
        .iter()
        .any(|row| row.operation_id == candidate.operation_id)
        || current
            .iter()
            .any(|row| row.stamp.happens_after(&candidate.stamp))
    {
        return;
    }
    current.retain(|row| !candidate.stamp.happens_after(&row.stamp));
    current.push(candidate);
}

fn push_revision<T>(current: &mut Vec<Revision<T>>, candidate: Revision<T>) {
    if !current
        .iter()
        .any(|row| row.operation_id == candidate.operation_id)
    {
        current.push(candidate);
    }
}

fn insert_crud<T>(current: &mut Vec<CrudRevision<T>>, candidate: CrudRevision<T>) {
    // A recreation does not erase its deletion's authority over still-offline edits.
    push_revision(current, candidate);
}

fn visible_maps<K: Ord, T>(maps: BTreeMap<K, Vec<CrudRevision<T>>>) -> BTreeMap<K, Vec<T>> {
    maps.into_iter()
        .filter_map(|(key, mut revisions)| {
            let deletes = revisions
                .iter()
                .filter(|row| matches!(row.value, CrudValue::Deleted))
                .map(|row| row.stamp.clone())
                .collect::<Vec<_>>();
            let mut visible = Vec::new();
            for row in revisions.drain(..) {
                if matches!(row.value, CrudValue::Present(_))
                    && deletes
                        .iter()
                        .all(|deleted| row.stamp.happens_after(deleted))
                {
                    insert_revision(&mut visible, row);
                }
            }
            sort_revisions(&mut visible);
            let values = visible
                .into_iter()
                .filter_map(|row| match row.value {
                    CrudValue::Present(value) => Some(value),
                    CrudValue::Deleted => None,
                })
                .collect::<Vec<_>>();
            (!values.is_empty()).then_some((key, values))
        })
        .collect()
}

fn apply_global_deletes<K: Ord, T>(
    maps: &mut BTreeMap<K, Vec<CrudRevision<T>>>,
    clears: &[Revision<()>],
) {
    for revisions in maps.values_mut() {
        for clear in clears {
            insert_crud(
                revisions,
                Revision {
                    stamp: clear.stamp.clone(),
                    operation_id: clear.operation_id.clone(),
                    value: CrudValue::Deleted,
                },
            );
        }
    }
}

fn apply_global_visit_deletes<K: Ord, T>(
    maps: &mut BTreeMap<K, Vec<CrudRevision<T>>>,
    clears: &[Revision<()>],
) {
    for revisions in maps.values_mut() {
        for clear in clears {
            push_revision(
                revisions,
                Revision {
                    stamp: clear.stamp.clone(),
                    operation_id: clear.operation_id.clone(),
                    value: CrudValue::Deleted,
                },
            );
        }
    }
}

fn merge_visits(mut revisions: Vec<CrudRevision<PassportVisit>>) -> Option<PassportVisit> {
    sort_revisions(&mut revisions);
    let deletes = revisions
        .iter()
        .filter(|row| matches!(&row.value, CrudValue::Deleted))
        .map(|row| row.stamp.clone())
        .collect::<Vec<_>>();
    let mut visits = revisions.into_iter().filter_map(|row| match row.value {
        CrudValue::Present(visit)
            if deletes.iter().all(|delete| row.stamp.happens_after(delete)) =>
        {
            Some(visit)
        }
        CrudValue::Present(_) | CrudValue::Deleted => None,
    });
    let mut merged = visits.next()?;
    for visit in visits {
        merged.first_listened_at_unix = merged
            .first_listened_at_unix
            .min(visit.first_listened_at_unix);
        merged.last_listened_at_unix = merged
            .last_listened_at_unix
            .max(visit.last_listened_at_unix);
        merged.station_name = visit.station_name;
        if visit.country_code.is_some() {
            merged.country_code = visit.country_code;
        }
    }
    Some(merged)
}

fn sort_revisions<T>(revisions: &mut [Revision<T>]) {
    revisions.sort_by(|left, right| {
        left.stamp
            .dot
            .cmp(&right.stamp.dot)
            .then(left.operation_id.cmp(&right.operation_id))
    });
}

fn validate_resume_device(
    envelope: &OperationEnvelope,
    device_id: &crate::personal_state::DeviceId,
) -> Result<(), PersonalStateError> {
    if &envelope.stamp.dot.device_id != device_id {
        return Err(PersonalStateError::InvalidOperation(
            "resume provenance does not match its causal device",
        ));
    }
    Ok(())
}
