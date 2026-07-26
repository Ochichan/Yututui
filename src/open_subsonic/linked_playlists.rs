//! Pure occurrence matching and write planning for linked OpenSubsonic playlists.
//!
//! OpenSubsonic identifies playlist members only by item ID and mutates playlists with
//! index-based removals followed by appends. YuTuTui keeps a permanent entry ID for every local
//! occurrence. This module bridges those representations without consulting playlist names,
//! clocks, credentials, storage, or the network.
//!
//! Conflict authority deliberately lives outside this module. A caller turns [`RemoteDelta`]
//! into personal-state entry operations, lets the causal reducer resolve concurrent edits, then
//! passes the resulting exact item order to [`plan_remote_update`].

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use crate::personal_state::PlaylistEntryId;

use super::ItemId;

/// Existing collection limit shared by local and linked server playlists.
pub const MAX_LINKED_PLAYLIST_ENTRIES: usize = 999;

/// One local occurrence that can be matched back to a server playlist occurrence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkedPlaylistEntry {
    pub entry_id: PlaylistEntryId,
    pub item_id: ItemId,
}

impl LinkedPlaylistEntry {
    pub fn new(entry_id: PlaylistEntryId, item_id: ItemId) -> Self {
        Self { entry_id, item_id }
    }
}

/// Identifies the bounded input that failed validation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaylistSequence {
    LocalEntries,
    PreviousRemoteShadow,
    CurrentRemote,
    DesiredRemote,
    RemoteReadback,
}

impl fmt::Display for PlaylistSequence {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::LocalEntries => "local entries",
            Self::PreviousRemoteShadow => "previous remote shadow",
            Self::CurrentRemote => "current remote playlist",
            Self::DesiredRemote => "desired remote playlist",
            Self::RemoteReadback => "remote playlist readback",
        })
    }
}

/// A bounded planning or readback-verification failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkedPlaylistError {
    TooManyEntries {
        sequence: PlaylistSequence,
        actual: usize,
        maximum: usize,
    },
    DuplicateEntryId {
        sequence: PlaylistSequence,
        entry_id: PlaylistEntryId,
    },
    EntryItemMismatch {
        entry_id: PlaylistEntryId,
    },
    ReadbackMismatch {
        expected_len: usize,
        actual_len: usize,
        first_difference: usize,
    },
}

impl fmt::Display for LinkedPlaylistError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooManyEntries {
                sequence,
                actual,
                maximum,
            } => write!(
                formatter,
                "{sequence} contains {actual} entries; the maximum is {maximum}"
            ),
            Self::DuplicateEntryId { sequence, entry_id } => write!(
                formatter,
                "{sequence} contains duplicate entry id {}",
                entry_id.as_str()
            ),
            Self::EntryItemMismatch { entry_id } => write!(
                formatter,
                "entry id {} refers to different exact item ids",
                entry_id.as_str()
            ),
            Self::ReadbackMismatch {
                expected_len,
                actual_len,
                first_difference,
            } => write!(
                formatter,
                "remote playlist readback differs at occurrence {first_difference} \
                 (expected {expected_len} entries, received {actual_len})"
            ),
        }
    }
}

impl std::error::Error for LinkedPlaylistError {}

/// A stable local/shadow occurrence matched to one exact remote occurrence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OccurrenceMatch {
    pub entry_index: usize,
    pub remote_index: usize,
    pub entry_id: PlaylistEntryId,
    pub item_id: ItemId,
}

/// An unmatched stable local/shadow occurrence, including its original position.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexedLinkedEntry {
    pub index: usize,
    pub entry: LinkedPlaylistEntry,
}

/// An unmatched remote occurrence, including its exact occurrence index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexedRemoteOccurrence {
    pub index: usize,
    pub item_id: ItemId,
}

/// Exact changes observed between the last verified server shadow and the current server order.
///
/// `removed` entries retain their stable local entry IDs. `added` entries intentionally do not
/// invent local IDs; the owning actor creates those before appending ledger operations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteDelta {
    pub retained: Vec<OccurrenceMatch>,
    pub removed: Vec<IndexedLinkedEntry>,
    pub added: Vec<IndexedRemoteOccurrence>,
}

/// Match a previous verified remote shadow to a current remote playlist.
///
/// Matching is an exact-item-ID LCS. Duplicate item IDs remain separate occurrences. Reconstruction
/// always takes an available exact diagonal, then skips a remote occurrence before a stable entry
/// when either skip preserves the same maximum length. That fixed rule makes every tie
/// deterministic on every device.
pub fn plan_remote_delta(
    previous_shadow: &[LinkedPlaylistEntry],
    current_remote: &[ItemId],
) -> Result<RemoteDelta, LinkedPlaylistError> {
    ensure_bounded(
        PlaylistSequence::PreviousRemoteShadow,
        previous_shadow.len(),
    )?;
    ensure_bounded(PlaylistSequence::CurrentRemote, current_remote.len())?;

    let mapping = exact_occurrence_mapping(previous_shadow, current_remote);
    Ok(RemoteDelta {
        retained: mapping.matches,
        removed: mapping.unmatched_entries,
        added: mapping.unmatched_remote,
    })
}

/// What the caller knows about delivery of the pending local server projection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PendingRemoteMergeMode {
    /// The pending write has not started. Equal local/remote additions are independent.
    LocalNotDelivered,
    /// The server accepted the complete desired projection before this readback.
    LocalDelivered,
    /// Delivery is unknown. Exact desired additions found remotely reuse their stable IDs.
    DeliveryUnknown,
}

/// One stable local occurrence preserved in a pending three-way merge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingExistingOccurrence {
    /// Position in the base shadow, or `None` when this was a local addition.
    pub base_index: Option<usize>,
    pub desired_index: usize,
    /// Exact position in the remote state, or `None` for an untransmitted local addition.
    pub remote_index: Option<usize>,
    pub entry: LinkedPlaylistEntry,
}

/// Provenance for one occurrence in a pending local/remote merged order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PendingMergeOccurrence {
    /// A base survivor, delivered local addition, or locally anchored addition.
    Existing(PendingExistingOccurrence),
    /// An occurrence created remotely after the base shadow. The caller assigns its local ID.
    RemoteOnly(IndexedRemoteOccurrence),
}

impl PendingMergeOccurrence {
    pub fn item_id(&self) -> &ItemId {
        match self {
            Self::Existing(existing) => &existing.entry.item_id,
            Self::RemoteOnly(remote) => &remote.item_id,
        }
    }
}

/// Provenance for one exact occurrence in the current server readback.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PendingRemoteOccurrence {
    /// The readback occurrence reuses a stable base or pending-local entry ID.
    Existing(PendingRemoteExistingOccurrence),
    /// The readback occurrence is a true remote addition and needs a new stable local ID.
    RemoteOnly(IndexedRemoteOccurrence),
}

/// One current server occurrence with a reusable stable entry ID.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingRemoteExistingOccurrence {
    pub base_index: Option<usize>,
    pub desired_index: Option<usize>,
    pub remote_index: usize,
    pub entry: LinkedPlaylistEntry,
}

/// Deterministic three-way result for an in-flight local projection and a changed server state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingRemoteMergePlan {
    ordered_occurrences: Vec<PendingMergeOccurrence>,
    remote_occurrences: Vec<PendingRemoteOccurrence>,
    removed_existing: Vec<IndexedLinkedEntry>,
    desired_remote: Vec<ItemId>,
}

impl PendingRemoteMergePlan {
    pub fn ordered_occurrences(&self) -> &[PendingMergeOccurrence] {
        &self.ordered_occurrences
    }

    /// Exact current server order. This always has one element per input remote occurrence.
    pub fn remote_occurrences(&self) -> &[PendingRemoteOccurrence] {
        &self.remote_occurrences
    }

    /// Stable desired entries removed by the remote side under the selected delivery policy.
    pub fn removed_existing(&self) -> &[IndexedLinkedEntry] {
        &self.removed_existing
    }

    pub fn desired_remote(&self) -> &[ItemId] {
        &self.desired_remote
    }
}

/// Merge an in-flight local projection with a concurrently changed remote playlist.
///
/// Base occurrences are first matched to exact remote occurrences by the same deterministic LCS
/// used by [`plan_remote_delta`], then unmatched equal-item occurrences are paired in base/remote
/// order. The latter step recognizes exact occurrence moves without treating titles or other
/// metadata as identity. A base occurrence survives only when both local and remote retained it,
/// so deletion on either side wins. Surviving base occurrences follow remote order.
///
/// Local additions attach to the nearest preceding surviving base occurrence in the pending local
/// order. Additions sharing an anchor are ordered by stable entry ID and are inserted before the
/// remote-only siblings following that anchor. This is independent of wall clocks and input map
/// iteration order.
pub fn plan_pending_remote_merge(
    previous_shadow: &[LinkedPlaylistEntry],
    desired: &[LinkedPlaylistEntry],
    current_remote: &[ItemId],
    mode: PendingRemoteMergeMode,
) -> Result<PendingRemoteMergePlan, LinkedPlaylistError> {
    ensure_bounded(
        PlaylistSequence::PreviousRemoteShadow,
        previous_shadow.len(),
    )?;
    ensure_bounded(PlaylistSequence::DesiredRemote, desired.len())?;
    ensure_bounded(PlaylistSequence::CurrentRemote, current_remote.len())?;
    validate_unique_entry_ids(PlaylistSequence::PreviousRemoteShadow, previous_shadow)?;
    validate_unique_entry_ids(PlaylistSequence::DesiredRemote, desired)?;

    let base_by_id = previous_shadow
        .iter()
        .enumerate()
        .map(|(index, entry)| (entry.entry_id.clone(), (index, entry)))
        .collect::<BTreeMap<_, _>>();
    let desired_by_id = desired
        .iter()
        .enumerate()
        .map(|(index, entry)| (entry.entry_id.clone(), (index, entry)))
        .collect::<BTreeMap<_, _>>();
    for (entry_id, (_, base_entry)) in &base_by_id {
        if let Some((_, desired_entry)) = desired_by_id.get(entry_id)
            && base_entry.item_id != desired_entry.item_id
        {
            return Err(LinkedPlaylistError::EntryItemMismatch {
                entry_id: entry_id.clone(),
            });
        }
    }

    let (ordered_occurrences, remote_occurrences) = match mode {
        PendingRemoteMergeMode::LocalDelivered => {
            merge_after_confirmed_delivery(desired, current_remote, &base_by_id)
        }
        PendingRemoteMergeMode::LocalNotDelivered | PendingRemoteMergeMode::DeliveryUnknown => {
            merge_before_or_unknown_delivery(
                previous_shadow,
                desired,
                current_remote,
                &base_by_id,
                &desired_by_id,
                mode,
            )
        }
    };
    ensure_bounded(PlaylistSequence::DesiredRemote, ordered_occurrences.len())?;
    let retained_entry_ids = ordered_occurrences
        .iter()
        .filter_map(|occurrence| match occurrence {
            PendingMergeOccurrence::Existing(existing) => Some(existing.entry.entry_id.clone()),
            PendingMergeOccurrence::RemoteOnly(_) => None,
        })
        .collect::<BTreeSet<_>>();
    let removed_existing = desired
        .iter()
        .enumerate()
        .filter(|(_, entry)| !retained_entry_ids.contains(&entry.entry_id))
        .map(|(index, entry)| IndexedLinkedEntry {
            index,
            entry: entry.clone(),
        })
        .collect();
    let desired_remote = ordered_occurrences
        .iter()
        .map(|occurrence| occurrence.item_id().clone())
        .collect();
    Ok(PendingRemoteMergePlan {
        ordered_occurrences,
        remote_occurrences,
        removed_existing,
        desired_remote,
    })
}

fn merge_after_confirmed_delivery(
    desired: &[LinkedPlaylistEntry],
    current_remote: &[ItemId],
    base_by_id: &BTreeMap<PlaylistEntryId, (usize, &LinkedPlaylistEntry)>,
) -> (Vec<PendingMergeOccurrence>, Vec<PendingRemoteOccurrence>) {
    let (_, remote_to_desired) = complete_base_remote_mapping(desired, current_remote);
    let mut ordered = Vec::with_capacity(current_remote.len());
    let mut remote_occurrences = Vec::with_capacity(current_remote.len());
    for (remote_index, item_id) in current_remote.iter().enumerate() {
        if let Some(desired_index) = remote_to_desired[remote_index] {
            let base_index = base_by_id
                .get(&desired[desired_index].entry_id)
                .map(|(index, _)| *index);
            let entry = desired[desired_index].clone();
            ordered.push(PendingMergeOccurrence::Existing(
                PendingExistingOccurrence {
                    base_index,
                    desired_index,
                    remote_index: Some(remote_index),
                    entry: entry.clone(),
                },
            ));
            remote_occurrences.push(PendingRemoteOccurrence::Existing(
                PendingRemoteExistingOccurrence {
                    base_index,
                    desired_index: Some(desired_index),
                    remote_index,
                    entry,
                },
            ));
        } else {
            let remote_only = IndexedRemoteOccurrence {
                index: remote_index,
                item_id: item_id.clone(),
            };
            ordered.push(PendingMergeOccurrence::RemoteOnly(remote_only.clone()));
            remote_occurrences.push(PendingRemoteOccurrence::RemoteOnly(remote_only));
        }
    }
    (ordered, remote_occurrences)
}

fn merge_before_or_unknown_delivery(
    previous_shadow: &[LinkedPlaylistEntry],
    desired: &[LinkedPlaylistEntry],
    current_remote: &[ItemId],
    base_by_id: &BTreeMap<PlaylistEntryId, (usize, &LinkedPlaylistEntry)>,
    desired_by_id: &BTreeMap<PlaylistEntryId, (usize, &LinkedPlaylistEntry)>,
    mode: PendingRemoteMergeMode,
) -> (Vec<PendingMergeOccurrence>, Vec<PendingRemoteOccurrence>) {
    let (base_to_remote, remote_to_base) =
        complete_base_remote_mapping(previous_shadow, current_remote);
    let surviving_base = previous_shadow
        .iter()
        .enumerate()
        .filter(|(base_index, entry)| {
            base_to_remote[*base_index].is_some() && desired_by_id.contains_key(&entry.entry_id)
        })
        .map(|(_, entry)| entry.entry_id.clone())
        .collect::<BTreeSet<_>>();

    let local_additions = desired
        .iter()
        .enumerate()
        .filter(|(_, entry)| !base_by_id.contains_key(&entry.entry_id))
        .map(|(index, entry)| IndexedLinkedEntry {
            index,
            entry: entry.clone(),
        })
        .collect::<Vec<_>>();
    let remote_available = remote_to_base
        .iter()
        .enumerate()
        .filter(|(_, base_index)| base_index.is_none())
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    let mut remote_to_local = vec![None; current_remote.len()];
    if mode == PendingRemoteMergeMode::DeliveryUnknown {
        let local_entries = local_additions
            .iter()
            .map(|addition| addition.entry.clone())
            .collect::<Vec<_>>();
        let available_items = remote_available
            .iter()
            .map(|index| current_remote[*index].clone())
            .collect::<Vec<_>>();
        let (_, available_to_local) =
            complete_base_remote_mapping(&local_entries, &available_items);
        for (available_index, local_index) in available_to_local.into_iter().enumerate() {
            if let Some(local_index) = local_index {
                remote_to_local[remote_available[available_index]] =
                    Some(local_additions[local_index].index);
            }
        }
    }
    let matched_local_ids = remote_to_local
        .iter()
        .flatten()
        .map(|desired_index| desired[*desired_index].entry_id.clone())
        .collect::<BTreeSet<_>>();
    let local_groups =
        group_local_additions(desired, base_by_id, &surviving_base, &matched_local_ids);

    let mut ordered = Vec::with_capacity(desired.len() + current_remote.len());
    let mut remote_occurrences = Vec::with_capacity(current_remote.len());
    append_local_group(&mut ordered, local_groups.get(&None).map(Vec::as_slice));
    for (remote_index, item_id) in current_remote.iter().enumerate() {
        if let Some(base_index) = remote_to_base[remote_index] {
            let base_entry = &previous_shadow[base_index];
            if !surviving_base.contains(&base_entry.entry_id) {
                remote_occurrences.push(PendingRemoteOccurrence::Existing(
                    PendingRemoteExistingOccurrence {
                        base_index: Some(base_index),
                        desired_index: None,
                        remote_index,
                        entry: base_entry.clone(),
                    },
                ));
                continue;
            }
            let (desired_index, desired_entry) = desired_by_id
                .get(&base_entry.entry_id)
                .expect("surviving base entry is present in desired");
            remote_occurrences.push(PendingRemoteOccurrence::Existing(
                PendingRemoteExistingOccurrence {
                    base_index: Some(base_index),
                    desired_index: Some(*desired_index),
                    remote_index,
                    entry: (*desired_entry).clone(),
                },
            ));
            ordered.push(PendingMergeOccurrence::Existing(
                PendingExistingOccurrence {
                    base_index: Some(base_index),
                    desired_index: *desired_index,
                    remote_index: Some(remote_index),
                    entry: (*desired_entry).clone(),
                },
            ));
            append_local_group(
                &mut ordered,
                local_groups
                    .get(&Some(base_entry.entry_id.clone()))
                    .map(Vec::as_slice),
            );
        } else if let Some(desired_index) = remote_to_local[remote_index] {
            remote_occurrences.push(PendingRemoteOccurrence::Existing(
                PendingRemoteExistingOccurrence {
                    base_index: None,
                    desired_index: Some(desired_index),
                    remote_index,
                    entry: desired[desired_index].clone(),
                },
            ));
            ordered.push(PendingMergeOccurrence::Existing(
                PendingExistingOccurrence {
                    base_index: None,
                    desired_index,
                    remote_index: Some(remote_index),
                    entry: desired[desired_index].clone(),
                },
            ));
        } else {
            let remote_only = IndexedRemoteOccurrence {
                index: remote_index,
                item_id: item_id.clone(),
            };
            ordered.push(PendingMergeOccurrence::RemoteOnly(remote_only.clone()));
            remote_occurrences.push(PendingRemoteOccurrence::RemoteOnly(remote_only));
        }
    }
    (ordered, remote_occurrences)
}

fn group_local_additions(
    desired: &[LinkedPlaylistEntry],
    base_by_id: &BTreeMap<PlaylistEntryId, (usize, &LinkedPlaylistEntry)>,
    surviving_base: &BTreeSet<PlaylistEntryId>,
    matched_local_ids: &BTreeSet<PlaylistEntryId>,
) -> BTreeMap<Option<PlaylistEntryId>, Vec<IndexedLinkedEntry>> {
    let mut groups = BTreeMap::<Option<PlaylistEntryId>, Vec<IndexedLinkedEntry>>::new();
    let mut anchor = None;
    for (index, entry) in desired.iter().enumerate() {
        if base_by_id.contains_key(&entry.entry_id) {
            if surviving_base.contains(&entry.entry_id) {
                anchor = Some(entry.entry_id.clone());
            }
        } else if !matched_local_ids.contains(&entry.entry_id) {
            groups
                .entry(anchor.clone())
                .or_default()
                .push(IndexedLinkedEntry {
                    index,
                    entry: entry.clone(),
                });
        }
    }
    for siblings in groups.values_mut() {
        siblings.sort_by(|left, right| left.entry.entry_id.cmp(&right.entry.entry_id));
    }
    groups
}

fn append_local_group(
    output: &mut Vec<PendingMergeOccurrence>,
    group: Option<&[IndexedLinkedEntry]>,
) {
    output.extend(group.into_iter().flatten().map(|local| {
        PendingMergeOccurrence::Existing(PendingExistingOccurrence {
            base_index: None,
            desired_index: local.index,
            remote_index: None,
            entry: local.entry.clone(),
        })
    }));
}

/// Counts shown before the first, deletion-free link is accepted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InitialMergePreview {
    pub add_to_local: usize,
    pub add_to_remote: usize,
}

/// The provenance of an occurrence in the first deletion-free merged order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InitialMergeOccurrence {
    /// One exact occurrence already exists on both sides.
    Matched(OccurrenceMatch),
    /// The actor must create a stable local entry ID for this server-only occurrence.
    RemoteOnly(IndexedRemoteOccurrence),
    /// This local-only occurrence is appended to the unchanged server order.
    LocalOnly(IndexedLinkedEntry),
}

impl InitialMergeOccurrence {
    pub fn item_id(&self) -> &ItemId {
        match self {
            Self::Matched(matched) => &matched.item_id,
            Self::RemoteOnly(remote) => &remote.item_id,
            Self::LocalOnly(local) => &local.entry.item_id,
        }
    }
}

/// First-link plan that preserves every local and remote occurrence without removing either side.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InitialMergePlan {
    preview: InitialMergePreview,
    ordered_occurrences: Vec<InitialMergeOccurrence>,
    desired_remote: Vec<ItemId>,
}

impl InitialMergePlan {
    pub fn preview(&self) -> InitialMergePreview {
        self.preview
    }

    pub fn ordered_occurrences(&self) -> &[InitialMergeOccurrence] {
        &self.ordered_occurrences
    }

    pub fn desired_remote(&self) -> &[ItemId] {
        &self.desired_remote
    }
}

/// Plan the first merge without deleting local or server occurrences.
///
/// The current server order is retained byte-for-byte. Exact LCS matches reuse stable local entry
/// IDs, server-only occurrences are imported in place, and unmatched local occurrences are
/// appended in their existing local order. Playlist names are not an input and can never create a
/// link or an occurrence match.
pub fn plan_initial_merge(
    local_entries: &[LinkedPlaylistEntry],
    current_remote: &[ItemId],
) -> Result<InitialMergePlan, LinkedPlaylistError> {
    ensure_bounded(PlaylistSequence::LocalEntries, local_entries.len())?;
    ensure_bounded(PlaylistSequence::CurrentRemote, current_remote.len())?;

    let mapping = exact_occurrence_mapping(local_entries, current_remote);
    let preview = InitialMergePreview {
        add_to_local: mapping.unmatched_remote.len(),
        add_to_remote: mapping.unmatched_entries.len(),
    };
    ensure_bounded(
        PlaylistSequence::DesiredRemote,
        current_remote.len() + mapping.unmatched_entries.len(),
    )?;
    let mut matches = mapping.matches.into_iter().peekable();
    let mut remote_only = mapping.unmatched_remote.into_iter().peekable();
    let mut ordered_occurrences =
        Vec::with_capacity(current_remote.len() + mapping.unmatched_entries.len());

    for remote_index in 0..current_remote.len() {
        if matches
            .peek()
            .is_some_and(|matched| matched.remote_index == remote_index)
        {
            ordered_occurrences.push(InitialMergeOccurrence::Matched(
                matches.next().expect("peeked match exists"),
            ));
        } else {
            debug_assert!(
                remote_only
                    .peek()
                    .is_some_and(|remote| remote.index == remote_index)
            );
            ordered_occurrences.push(InitialMergeOccurrence::RemoteOnly(
                remote_only
                    .next()
                    .expect("unmatched remote occurrence exists"),
            ));
        }
    }

    ordered_occurrences.extend(
        mapping
            .unmatched_entries
            .into_iter()
            .map(InitialMergeOccurrence::LocalOnly),
    );
    let desired_remote = ordered_occurrences
        .iter()
        .map(|occurrence| occurrence.item_id().clone())
        .collect();

    Ok(InitialMergePlan {
        preview,
        ordered_occurrences,
        desired_remote,
    })
}

/// A server-compatible update plan.
///
/// OpenSubsonic removals are index-based and additions append to the end. The planner therefore
/// retains the longest prefix of `desired` that is a subsequence of `current`, removes every
/// other current occurrence in descending index order, then appends the remaining desired suffix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemotePlaylistUpdatePlan {
    retained_current_indexes: Vec<usize>,
    remove_indexes_descending: Vec<usize>,
    append_item_ids: Vec<ItemId>,
    desired: Vec<ItemId>,
}

impl RemotePlaylistUpdatePlan {
    pub fn retained_current_indexes(&self) -> &[usize] {
        &self.retained_current_indexes
    }

    pub fn remove_indexes_descending(&self) -> &[usize] {
        &self.remove_indexes_descending
    }

    pub fn append_item_ids(&self) -> &[ItemId] {
        &self.append_item_ids
    }

    pub fn desired(&self) -> &[ItemId] {
        &self.desired
    }

    pub fn is_noop(&self) -> bool {
        self.remove_indexes_descending.is_empty() && self.append_item_ids.is_empty()
    }

    /// Accept a write only after a complete playlist readback exactly matches the desired order.
    pub fn verify_readback(&self, actual: &[ItemId]) -> Result<(), LinkedPlaylistError> {
        ensure_bounded(PlaylistSequence::RemoteReadback, actual.len())?;
        if self.desired == actual {
            return Ok(());
        }

        let first_difference = self
            .desired
            .iter()
            .zip(actual)
            .position(|(expected, received)| expected != received)
            .unwrap_or_else(|| self.desired.len().min(actual.len()));
        Err(LinkedPlaylistError::ReadbackMismatch {
            expected_len: self.desired.len(),
            actual_len: actual.len(),
            first_difference,
        })
    }
}

pub fn plan_remote_update(
    current: &[ItemId],
    desired: &[ItemId],
) -> Result<RemotePlaylistUpdatePlan, LinkedPlaylistError> {
    ensure_bounded(PlaylistSequence::CurrentRemote, current.len())?;
    ensure_bounded(PlaylistSequence::DesiredRemote, desired.len())?;

    let mut retained_current_indexes = Vec::new();
    let mut search_from = 0;
    for desired_item in desired {
        let Some(relative_index) = current[search_from..]
            .iter()
            .position(|current_item| current_item == desired_item)
        else {
            break;
        };
        let current_index = search_from + relative_index;
        retained_current_indexes.push(current_index);
        search_from = current_index + 1;
    }

    let mut retained = vec![false; current.len()];
    for &index in &retained_current_indexes {
        retained[index] = true;
    }
    let remove_indexes_descending = (0..current.len())
        .rev()
        .filter(|index| !retained[*index])
        .collect();
    let append_item_ids = desired[retained_current_indexes.len()..].to_vec();

    Ok(RemotePlaylistUpdatePlan {
        retained_current_indexes,
        remove_indexes_descending,
        append_item_ids,
        desired: desired.to_vec(),
    })
}

struct ExactOccurrenceMapping {
    matches: Vec<OccurrenceMatch>,
    unmatched_entries: Vec<IndexedLinkedEntry>,
    unmatched_remote: Vec<IndexedRemoteOccurrence>,
}

fn validate_unique_entry_ids(
    sequence: PlaylistSequence,
    entries: &[LinkedPlaylistEntry],
) -> Result<(), LinkedPlaylistError> {
    let mut seen = BTreeSet::new();
    for entry in entries {
        if !seen.insert(entry.entry_id.clone()) {
            return Err(LinkedPlaylistError::DuplicateEntryId {
                sequence,
                entry_id: entry.entry_id.clone(),
            });
        }
    }
    Ok(())
}

/// Extend the deterministic LCS anchors with exact residual occurrence pairing.
///
/// LCS establishes the same stable anchors used by normal remote observation. Remaining equal
/// items are paired in base order with remote order, recognizing moves while leaving duplicate
/// occurrences distinct. Entries and remote positions without an exact counterpart remain `None`.
fn complete_base_remote_mapping(
    entries: &[LinkedPlaylistEntry],
    remote: &[ItemId],
) -> (Vec<Option<usize>>, Vec<Option<usize>>) {
    let mapping = exact_occurrence_mapping(entries, remote);
    let mut entry_to_remote = vec![None; entries.len()];
    let mut remote_to_entry = vec![None; remote.len()];
    for matched in &mapping.matches {
        entry_to_remote[matched.entry_index] = Some(matched.remote_index);
        remote_to_entry[matched.remote_index] = Some(matched.entry_index);
    }

    let mut remote_by_item = BTreeMap::<ItemId, Vec<usize>>::new();
    for occurrence in mapping.unmatched_remote.iter().rev() {
        remote_by_item
            .entry(occurrence.item_id.clone())
            .or_default()
            .push(occurrence.index);
    }
    for unmatched in mapping.unmatched_entries {
        let Some(remote_index) = remote_by_item
            .get_mut(&unmatched.entry.item_id)
            .and_then(Vec::pop)
        else {
            continue;
        };
        entry_to_remote[unmatched.index] = Some(remote_index);
        remote_to_entry[remote_index] = Some(unmatched.index);
    }
    (entry_to_remote, remote_to_entry)
}

fn exact_occurrence_mapping(
    entries: &[LinkedPlaylistEntry],
    remote: &[ItemId],
) -> ExactOccurrenceMapping {
    let pairs = deterministic_lcs_pairs(entries, remote);
    let mut matched_entries = vec![false; entries.len()];
    let mut matched_remote = vec![false; remote.len()];
    let matches = pairs
        .into_iter()
        .map(|(entry_index, remote_index)| {
            matched_entries[entry_index] = true;
            matched_remote[remote_index] = true;
            OccurrenceMatch {
                entry_index,
                remote_index,
                entry_id: entries[entry_index].entry_id.clone(),
                item_id: entries[entry_index].item_id.clone(),
            }
        })
        .collect();
    let unmatched_entries = entries
        .iter()
        .enumerate()
        .filter(|(index, _)| !matched_entries[*index])
        .map(|(index, entry)| IndexedLinkedEntry {
            index,
            entry: entry.clone(),
        })
        .collect();
    let unmatched_remote = remote
        .iter()
        .enumerate()
        .filter(|(index, _)| !matched_remote[*index])
        .map(|(index, item_id)| IndexedRemoteOccurrence {
            index,
            item_id: item_id.clone(),
        })
        .collect();

    ExactOccurrenceMapping {
        matches,
        unmatched_entries,
        unmatched_remote,
    }
}

fn deterministic_lcs_pairs(
    entries: &[LinkedPlaylistEntry],
    remote: &[ItemId],
) -> Vec<(usize, usize)> {
    let stride = remote.len() + 1;
    let mut lengths = vec![0usize; (entries.len() + 1) * stride];
    let at = |entry_index: usize, remote_index: usize| entry_index * stride + remote_index;

    for entry_index in (0..entries.len()).rev() {
        for remote_index in (0..remote.len()).rev() {
            lengths[at(entry_index, remote_index)] =
                if entries[entry_index].item_id == remote[remote_index] {
                    1 + lengths[at(entry_index + 1, remote_index + 1)]
                } else {
                    lengths[at(entry_index + 1, remote_index)]
                        .max(lengths[at(entry_index, remote_index + 1)])
                };
        }
    }

    let mut pairs = Vec::with_capacity(lengths[at(0, 0)]);
    let mut entry_index = 0;
    let mut remote_index = 0;
    while entry_index < entries.len() && remote_index < remote.len() {
        let best = lengths[at(entry_index, remote_index)];
        if entries[entry_index].item_id == remote[remote_index]
            && best == 1 + lengths[at(entry_index + 1, remote_index + 1)]
        {
            pairs.push((entry_index, remote_index));
            entry_index += 1;
            remote_index += 1;
        } else if lengths[at(entry_index, remote_index + 1)] == best {
            // If both skips keep a maximum LCS, this fixed direction is the deterministic tie
            // break used on every device.
            remote_index += 1;
        } else {
            entry_index += 1;
        }
    }
    pairs
}

fn ensure_bounded(sequence: PlaylistSequence, actual: usize) -> Result<(), LinkedPlaylistError> {
    if actual <= MAX_LINKED_PLAYLIST_ENTRIES {
        Ok(())
    } else {
        Err(LinkedPlaylistError::TooManyEntries {
            sequence,
            actual,
            maximum: MAX_LINKED_PLAYLIST_ENTRIES,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(value: &str) -> ItemId {
        ItemId::new(value).unwrap()
    }

    fn entry(entry_id: &str, item_id: &str) -> LinkedPlaylistEntry {
        LinkedPlaylistEntry::new(PlaylistEntryId::new(entry_id).unwrap(), item(item_id))
    }

    fn item_values(items: &[ItemId]) -> Vec<&str> {
        items.iter().map(ItemId::as_str).collect()
    }

    fn apply_update_plan(current: &[ItemId], plan: &RemotePlaylistUpdatePlan) -> Vec<ItemId> {
        let mut result = current.to_vec();
        for &index in plan.remove_indexes_descending() {
            result.remove(index);
        }
        result.extend_from_slice(plan.append_item_ids());
        result
    }

    #[test]
    fn exact_lcs_preserves_duplicate_occurrences() {
        let shadow = [entry("one", "a"), entry("two", "a"), entry("three", "b")];
        let current = [item("a"), item("b"), item("a")];

        let delta = plan_remote_delta(&shadow, &current).unwrap();

        assert_eq!(
            delta
                .retained
                .iter()
                .map(|matched| (matched.entry_index, matched.remote_index))
                .collect::<Vec<_>>(),
            vec![(0, 0), (1, 2)]
        );
        assert_eq!(
            delta
                .removed
                .iter()
                .map(|removed| removed.entry.entry_id.as_str())
                .collect::<Vec<_>>(),
            vec!["three"]
        );
        assert_eq!(
            delta
                .added
                .iter()
                .map(|added| (added.index, added.item_id.as_str()))
                .collect::<Vec<_>>(),
            vec![(1, "b")]
        );
    }

    #[test]
    fn reorder_is_an_exact_remove_and_add_not_a_fuzzy_match() {
        let shadow = [entry("a-entry", "a"), entry("b-entry", "b")];
        let current = [item("b"), item("a")];

        let delta = plan_remote_delta(&shadow, &current).unwrap();

        assert_eq!(delta.retained.len(), 1);
        assert_eq!(delta.retained[0].entry_id.as_str(), "a-entry");
        assert_eq!(delta.retained[0].remote_index, 1);
        assert_eq!(delta.removed[0].entry.entry_id.as_str(), "b-entry");
        assert_eq!(delta.added[0].item_id.as_str(), "b");
        assert_eq!(delta.added[0].index, 0);
    }

    #[test]
    fn lcs_ties_use_the_fixed_remote_skip_before_entry_skip() {
        let shadow = [entry("first", "a"), entry("second", "b")];
        let reordered = [item("b"), item("a")];
        let delta = plan_remote_delta(&shadow, &reordered).unwrap();
        assert_eq!(
            delta
                .retained
                .iter()
                .map(|matched| (matched.entry_index, matched.remote_index))
                .collect::<Vec<_>>(),
            vec![(0, 1)]
        );

        let duplicate_shadow = [entry("first", "a"), entry("second", "a")];
        let one_remote = [item("a")];
        let delta = plan_remote_delta(&duplicate_shadow, &one_remote).unwrap();
        assert_eq!(delta.retained[0].entry_index, 0);

        let one_shadow = [entry("only", "a")];
        let duplicate_remote = [item("a"), item("a")];
        let delta = plan_remote_delta(&one_shadow, &duplicate_remote).unwrap();
        assert_eq!(delta.retained[0].remote_index, 0);
    }

    #[test]
    fn simultaneous_remote_add_and_remove_keep_stable_ids_for_the_reducer() {
        let shadow = [
            entry("first", "a"),
            entry("removed", "b"),
            entry("last", "c"),
        ];
        let current = [item("a"), item("new"), item("c")];

        let delta = plan_remote_delta(&shadow, &current).unwrap();

        assert_eq!(
            delta
                .retained
                .iter()
                .map(|matched| matched.entry_id.as_str())
                .collect::<Vec<_>>(),
            vec!["first", "last"]
        );
        assert_eq!(delta.removed[0].entry.entry_id.as_str(), "removed");
        assert_eq!(delta.added[0].index, 1);
        assert_eq!(delta.added[0].item_id.as_str(), "new");
    }

    #[test]
    fn first_merge_is_deletion_free_and_reports_both_counts() {
        let local = [
            entry("first-a", "a"),
            entry("second-a", "a"),
            entry("local-c", "c"),
        ];
        let remote = [item("a"), item("b"), item("a")];

        let plan = plan_initial_merge(&local, &remote).unwrap();

        assert_eq!(
            plan.preview(),
            InitialMergePreview {
                add_to_local: 1,
                add_to_remote: 1,
            }
        );
        assert_eq!(item_values(plan.desired_remote()), vec!["a", "b", "a", "c"]);
        assert!(matches!(
            plan.ordered_occurrences(),
            [
                InitialMergeOccurrence::Matched(_),
                InitialMergeOccurrence::RemoteOnly(_),
                InitialMergeOccurrence::Matched(_),
                InitialMergeOccurrence::LocalOnly(_)
            ]
        ));
    }

    #[test]
    fn first_merge_does_not_collapse_equal_item_ids_or_consult_names() {
        let local = [entry("local-first", "same"), entry("local-second", "same")];
        let remote = [item("same")];

        let plan = plan_initial_merge(&local, &remote).unwrap();

        assert_eq!(plan.preview().add_to_local, 0);
        assert_eq!(plan.preview().add_to_remote, 1);
        assert_eq!(item_values(plan.desired_remote()), vec!["same", "same"]);
        assert_eq!(
            plan.ordered_occurrences()
                .iter()
                .filter(|occurrence| matches!(occurrence, InitialMergeOccurrence::Matched(_)))
                .count(),
            1
        );
    }

    #[test]
    fn first_merge_handles_empty_sides_without_deletions() {
        let local = [entry("local", "a")];
        let no_remote = plan_initial_merge(&local, &[]).unwrap();
        assert_eq!(
            no_remote.preview(),
            InitialMergePreview {
                add_to_local: 0,
                add_to_remote: 1,
            }
        );
        assert_eq!(item_values(no_remote.desired_remote()), vec!["a"]);

        let remote = [item("b")];
        let no_local = plan_initial_merge(&[], &remote).unwrap();
        assert_eq!(
            no_local.preview(),
            InitialMergePreview {
                add_to_local: 1,
                add_to_remote: 0,
            }
        );
        assert_eq!(item_values(no_local.desired_remote()), vec!["b"]);
    }

    #[test]
    fn remote_update_retains_longest_desired_prefix_subsequence() {
        let current = [item("a"), item("x"), item("b"), item("y"), item("c")];
        let desired = [item("a"), item("b"), item("d")];

        let plan = plan_remote_update(&current, &desired).unwrap();

        assert_eq!(plan.retained_current_indexes(), &[0, 2]);
        assert_eq!(plan.remove_indexes_descending(), &[4, 3, 1]);
        assert_eq!(item_values(plan.append_item_ids()), vec!["d"]);
        assert_eq!(apply_update_plan(&current, &plan), desired);
    }

    #[test]
    fn remote_update_handles_duplicate_reorder_deterministically() {
        let current = [item("a"), item("b"), item("a"), item("c")];
        let desired = [item("a"), item("a"), item("b"), item("a")];

        let plan = plan_remote_update(&current, &desired).unwrap();

        assert_eq!(plan.retained_current_indexes(), &[0, 2]);
        assert_eq!(plan.remove_indexes_descending(), &[3, 1]);
        assert_eq!(item_values(plan.append_item_ids()), vec!["b", "a"]);
        assert_eq!(apply_update_plan(&current, &plan), desired);
    }

    #[test]
    fn remote_update_noop_and_remove_all_are_explicit() {
        let current = [item("a"), item("b")];
        let noop = plan_remote_update(&current, &current).unwrap();
        assert!(noop.is_noop());
        assert_eq!(noop.retained_current_indexes(), &[0, 1]);
        assert!(noop.remove_indexes_descending().is_empty());
        assert!(noop.append_item_ids().is_empty());

        let remove_all = plan_remote_update(&current, &[]).unwrap();
        assert!(!remove_all.is_noop());
        assert_eq!(remove_all.remove_indexes_descending(), &[1, 0]);
        assert!(remove_all.append_item_ids().is_empty());
        assert!(apply_update_plan(&current, &remove_all).is_empty());
    }

    #[test]
    fn full_readback_requires_exact_order_and_duplicate_count() {
        let current = [item("a")];
        let desired = [item("a"), item("b"), item("a")];
        let plan = plan_remote_update(&current, &desired).unwrap();

        assert_eq!(plan.verify_readback(&desired), Ok(()));
        assert_eq!(
            plan.verify_readback(&[item("a"), item("a"), item("b")]),
            Err(LinkedPlaylistError::ReadbackMismatch {
                expected_len: 3,
                actual_len: 3,
                first_difference: 1,
            })
        );
        assert_eq!(
            plan.verify_readback(&[item("a"), item("b")]),
            Err(LinkedPlaylistError::ReadbackMismatch {
                expected_len: 3,
                actual_len: 2,
                first_difference: 2,
            })
        );
    }

    #[test]
    fn every_planner_input_rejects_more_than_999_occurrences() {
        let too_many_items = vec![item("same"); MAX_LINKED_PLAYLIST_ENTRIES + 1];
        let too_many_entries = (0..=MAX_LINKED_PLAYLIST_ENTRIES)
            .map(|index| entry(&format!("entry-{index}"), "same"))
            .collect::<Vec<_>>();

        assert_eq!(
            plan_initial_merge(&too_many_entries, &[]),
            Err(LinkedPlaylistError::TooManyEntries {
                sequence: PlaylistSequence::LocalEntries,
                actual: 1000,
                maximum: 999,
            })
        );
        assert_eq!(
            plan_initial_merge(&[], &too_many_items),
            Err(LinkedPlaylistError::TooManyEntries {
                sequence: PlaylistSequence::CurrentRemote,
                actual: 1000,
                maximum: 999,
            })
        );
        let local_half = (0..600)
            .map(|index| entry(&format!("local-{index}"), &format!("local-item-{index}")))
            .collect::<Vec<_>>();
        let remote_half = (0..600)
            .map(|index| item(&format!("remote-item-{index}")))
            .collect::<Vec<_>>();
        assert_eq!(
            plan_initial_merge(&local_half, &remote_half),
            Err(LinkedPlaylistError::TooManyEntries {
                sequence: PlaylistSequence::DesiredRemote,
                actual: 1200,
                maximum: 999,
            })
        );
        assert_eq!(
            plan_remote_delta(&too_many_entries, &[]),
            Err(LinkedPlaylistError::TooManyEntries {
                sequence: PlaylistSequence::PreviousRemoteShadow,
                actual: 1000,
                maximum: 999,
            })
        );
        assert_eq!(
            plan_remote_update(&too_many_items, &[]),
            Err(LinkedPlaylistError::TooManyEntries {
                sequence: PlaylistSequence::CurrentRemote,
                actual: 1000,
                maximum: 999,
            })
        );
        assert_eq!(
            plan_remote_update(&[], &too_many_items),
            Err(LinkedPlaylistError::TooManyEntries {
                sequence: PlaylistSequence::DesiredRemote,
                actual: 1000,
                maximum: 999,
            })
        );

        let plan = plan_remote_update(&[], &[]).unwrap();
        assert_eq!(
            plan.verify_readback(&too_many_items),
            Err(LinkedPlaylistError::TooManyEntries {
                sequence: PlaylistSequence::RemoteReadback,
                actual: 1000,
                maximum: 999,
            })
        );
    }

    #[test]
    fn exact_limit_is_accepted() {
        let items = vec![item("same"); MAX_LINKED_PLAYLIST_ENTRIES];
        let entries = (0..MAX_LINKED_PLAYLIST_ENTRIES)
            .map(|index| entry(&format!("entry-{index}"), "same"))
            .collect::<Vec<_>>();

        let initial = plan_initial_merge(&entries, &items).unwrap();
        assert_eq!(initial.desired_remote().len(), MAX_LINKED_PLAYLIST_ENTRIES);
        let delta = plan_remote_delta(&entries, &items).unwrap();
        assert_eq!(delta.retained.len(), MAX_LINKED_PLAYLIST_ENTRIES);
        let update = plan_remote_update(&items, &items).unwrap();
        assert!(update.is_noop());
        assert_eq!(update.verify_readback(&items), Ok(()));
    }
}

#[cfg(test)]
#[path = "linked_playlists/pending_merge_tests.rs"]
mod pending_merge_tests;
