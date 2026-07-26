//! Durable bounded progress for exact server-history imports.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use super::{
    BridgeMutationError, MAX_CURSOR_KEY_BYTES, MAX_EVENT_ID_BYTES, MAX_HISTORY_CURSORS,
    OpenSubsonicBridgeState, validate_identifier,
};

/// Resume point for a bounded exact-history importer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct HistoryCursor {
    pub high_water_id: Option<String>,
    pub overlap_started_at_unix: Option<i64>,
    pub updated_at_unix: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub continuation: Option<HistoryContinuation>,
    /// Exact rows whose metadata must be resolved before scanning advances again.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pending_metadata_rows: Vec<PendingNativeMetadataRow>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PendingNativeMetadataRow {
    pub row_id: u64,
    pub item_id: super::ItemId,
    pub observed_at_unix: i64,
}

/// Durable progress for a scan that exhausted one bounded refresh.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct HistoryContinuation {
    pub candidate_high_water_id: Option<String>,
    pub next_start: u32,
    pub through_unix: Option<u64>,
    #[serde(default)]
    pub reached_high_water: bool,
    /// IDs from the preceding page that an inclusive timestamp boundary can return again.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub overlap_row_ids: Vec<u64>,
    #[serde(default)]
    pub backlog_complete: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head_anchor_high_water_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head_next_start: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head_from_unix: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head_through_unix: Option<u64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub head_overlap_row_ids: Vec<u64>,
}

/// Redacted capability/health for the optional native-history bridge.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NativeHistoryHealth {
    #[default]
    Off,
    Probing,
    Detailed,
    PlayCountsOnly,
    UpdatePassword,
}

impl OpenSubsonicBridgeState {
    pub(crate) fn history_cursors(&self) -> &std::collections::BTreeMap<String, HistoryCursor> {
        &self.history_cursors
    }

    pub(crate) fn set_history_cursor(
        &mut self,
        source: String,
        cursor: HistoryCursor,
    ) -> Result<(), BridgeMutationError> {
        validate_identifier(&source, MAX_CURSOR_KEY_BYTES)?;
        validate(&cursor)?;
        if !self.history_cursors.contains_key(&source)
            && self.history_cursors.len() >= MAX_HISTORY_CURSORS
        {
            return Err(BridgeMutationError::CapacityExceeded);
        }
        self.history_cursors.insert(source, cursor);
        Ok(())
    }
}

pub(super) fn validate(cursor: &HistoryCursor) -> Result<(), BridgeMutationError> {
    if let Some(high_water_id) = &cursor.high_water_id {
        validate_identifier(high_water_id, MAX_EVENT_ID_BYTES)?;
    }
    if let Some(continuation) = &cursor.continuation {
        if let Some(candidate) = &continuation.candidate_high_water_id {
            validate_identifier(candidate, MAX_EVENT_ID_BYTES)?;
        }
        if let Some(anchor) = &continuation.head_anchor_high_water_id {
            validate_identifier(anchor, MAX_EVENT_ID_BYTES)?;
        }
        if continuation.next_start as usize
            >= super::super::native_history::MAX_NATIVE_HISTORY_OFFSET
        {
            return Err(BridgeMutationError::CapacityExceeded);
        }
        if continuation.head_next_start.is_some_and(|next_start| {
            next_start as usize >= super::super::native_history::MAX_NATIVE_HISTORY_OFFSET
        }) {
            return Err(BridgeMutationError::CapacityExceeded);
        }
        validate_overlap(&continuation.overlap_row_ids)?;
        validate_overlap(&continuation.head_overlap_row_ids)?;
        if continuation.head_next_start.is_none()
            && (continuation.head_anchor_high_water_id.is_some()
                || continuation.head_from_unix.is_some()
                || continuation.head_through_unix.is_some()
                || !continuation.head_overlap_row_ids.is_empty())
        {
            return Err(BridgeMutationError::ConflictingEntry);
        }
        if matches!(
            (
                continuation.head_from_unix,
                continuation.head_through_unix
            ),
            (Some(from), Some(through)) if from > through
        ) {
            return Err(BridgeMutationError::ConflictingEntry);
        }
    }
    if cursor.pending_metadata_rows.len() > super::super::native_history::MAX_NATIVE_HISTORY_ROWS {
        return Err(BridgeMutationError::CapacityExceeded);
    }
    let mut row_ids = BTreeSet::new();
    if cursor
        .pending_metadata_rows
        .iter()
        .any(|row| !row_ids.insert(row.row_id))
    {
        return Err(BridgeMutationError::ConflictingEntry);
    }
    Ok(())
}

fn validate_overlap(ids: &[u64]) -> Result<(), BridgeMutationError> {
    if ids.len() > super::super::native_history::NATIVE_HISTORY_PAGE_SIZE {
        return Err(BridgeMutationError::CapacityExceeded);
    }
    let mut unique = BTreeSet::new();
    if ids.iter().any(|id| !unique.insert(id)) {
        return Err(BridgeMutationError::ConflictingEntry);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pending_metadata_rows_survive_a_restart_round_trip() {
        let cursor = HistoryCursor {
            high_water_id: Some("65".to_owned()),
            overlap_started_at_unix: Some(1),
            updated_at_unix: 2,
            continuation: None,
            pending_metadata_rows: (1..=65)
                .rev()
                .map(|id| PendingNativeMetadataRow {
                    row_id: id,
                    item_id: super::super::ItemId::new(format!("song-{id}")).unwrap(),
                    observed_at_unix: i64::try_from(id).unwrap(),
                })
                .collect(),
        };

        let encoded = serde_json::to_vec(&cursor).unwrap();
        let restored: HistoryCursor = serde_json::from_slice(&encoded).unwrap();

        assert_eq!(restored, cursor);
        validate(&restored).unwrap();
    }

    #[test]
    fn older_cursor_json_defaults_to_no_pending_metadata() {
        let restored: HistoryCursor = serde_json::from_str(
            r#"{
                "high_water_id":"7",
                "overlap_started_at_unix":1,
                "updated_at_unix":2,
                "continuation":null
            }"#,
        )
        .unwrap();

        assert!(restored.pending_metadata_rows.is_empty());
    }
}
