//! Offline owner-store recovery for ambiguously delivered playback reports.
//!
//! These functions never start an actor or perform network I/O. Mutating callers must already own
//! the process-wide persistence writer lease.

use super::actor::{OutboundScrobbleResolution, ServiceError};
use super::bridge_runtime::BridgeRuntime;
use super::profile::OpenSubsonicPaths;
use super::transaction::{load_store_set, load_store_set_read_only};

const OPAQUE_EVENT_ID_PREFIX: &str = "sub-scrobble-";
const OPAQUE_EVENT_ID_HEX_LEN: usize = 64;

/// Return only opaque report IDs from one coherent read-only snapshot.
pub fn list_scrobble_attention_ids(paths: &OpenSubsonicPaths) -> Result<Vec<String>, ServiceError> {
    let store_set = load_store_set_read_only(paths)?.ok_or(ServiceError::InvalidSetup)?;
    let ids = store_set.bridge_state.outbound_scrobble_attention_ids();
    if ids.iter().all(|id| is_opaque_event_id(id)) {
        Ok(ids)
    } else {
        Err(ServiceError::InvalidSetup)
    }
}

/// Persist one explicit delivery decision without contacting the configured server.
pub fn resolve_scrobble_attention(
    paths: &OpenSubsonicPaths,
    event_id: &str,
    resolution: OutboundScrobbleResolution,
) -> Result<(), ServiceError> {
    if !is_opaque_event_id(event_id) {
        return Err(ServiceError::InvalidSetup);
    }
    let mut store_set = load_store_set(paths)?.ok_or(ServiceError::InvalidSetup)?;
    BridgeRuntime::writable(paths.clone(), None).resolve_outbound_scrobble(
        &mut store_set,
        event_id,
        resolution,
    )
}

fn is_opaque_event_id(value: &str) -> bool {
    value
        .strip_prefix(OPAQUE_EVENT_ID_PREFIX)
        .is_some_and(|digest| {
            digest.len() == OPAQUE_EVENT_ID_HEX_LEN
                && digest
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
        })
}

#[cfg(test)]
mod tests {
    use age::secrecy::SecretString;

    use super::*;
    use crate::open_subsonic::bridge_store::{
        AggregatePlayShadow, MAX_UNCERTAIN_SCROBBLE_READBACKS, OutboundScrobbleDelivery,
        OutboundScrobbleKind, PendingOutboundScrobble,
    };
    use crate::open_subsonic::{
        ConfiguredPrivateOrigin, ItemId, OpenSubsonicBridgeState, OpenSubsonicPrivateState,
        OpenSubsonicProfile, OpenSubsonicStoreSet, ServerCredential, StoreRevisions,
        commit_store_set,
    };

    fn fixture(label: &str) -> (std::path::PathBuf, OpenSubsonicPaths, String) {
        let root = std::env::temp_dir().join(format!(
            "yututui-outbound-recovery-{label}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        crate::persist::initialize_persistence_writer_for_roots([&root], false).unwrap();
        crate::util::safe_fs::ensure_private_dir(&root).unwrap();
        let paths = OpenSubsonicPaths::for_data_root(root.clone());
        let profile = OpenSubsonicProfile::new(
            "Recovery fixture",
            ConfiguredPrivateOrigin::new("http://127.0.0.1:9/", true).unwrap(),
            None,
        )
        .unwrap();
        let private_state = OpenSubsonicPrivateState::new(
            profile.backend_id().clone(),
            profile.account_scope_id().clone(),
            ServerCredential::api_key(SecretString::from("recovery-secret".to_owned())).unwrap(),
        );
        let mut bridge_state = OpenSubsonicBridgeState::new(
            profile.backend_id().clone(),
            profile.account_scope_id().clone(),
        );
        let item_id = ItemId::new("song").unwrap();
        bridge_state
            .reserve_outbound_exact_history_credit(item_id.clone(), 0)
            .unwrap();
        let event_id = format!("sub-scrobble-{}", "a".repeat(64));
        bridge_state
            .queue_outbound_scrobble(PendingOutboundScrobble {
                event_id: event_id.clone(),
                item_id,
                played_at_unix: 42,
                kind: OutboundScrobbleKind::Submission,
                delivery: OutboundScrobbleDelivery::NeedsAttention,
                baseline_captured: true,
                baseline_play_count: Some(7),
                baseline_played_at: None,
                exact_credit_recorded: true,
                exact_credit_epoch: Some(0),
                uncertain_readbacks: MAX_UNCERTAIN_SCROBBLE_READBACKS,
                source_marker_acknowledged: false,
            })
            .unwrap();
        let mut store_set =
            OpenSubsonicStoreSet::new(profile, private_state, bridge_state).unwrap();
        commit_store_set(&paths, StoreRevisions::MISSING, &mut store_set).unwrap();
        (root, paths, event_id)
    }

    #[test]
    fn offline_list_and_mark_sent_return_only_opaque_identity_and_preserve_credit() {
        let (root, paths, event_id) = fixture("mark-sent");
        assert_eq!(
            list_scrobble_attention_ids(&paths).unwrap(),
            vec![event_id.clone()]
        );

        resolve_scrobble_attention(&paths, &event_id, OutboundScrobbleResolution::MarkSent)
            .unwrap();
        let durable = load_store_set(&paths).unwrap().unwrap();
        assert!(durable.bridge_state.outbound_scrobbles().is_empty());
        assert_eq!(durable.bridge_state.completed_outbound_scrobbles().len(), 1);
        assert_eq!(durable.bridge_state.outbound_echoes().len(), 1);
        assert_eq!(
            durable
                .bridge_state
                .history_dedupe_credits()
                .get(&ItemId::new("song").unwrap())
                .unwrap()
                .get(&0)
                .unwrap()
                .exact_unmatched,
            1
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn recovery_boundary_rejects_non_opaque_identifiers() {
        for id in [
            "owner-event-with-metadata",
            "sub-scrobble-short",
            "sub-scrobble-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
        ] {
            assert!(!is_opaque_event_id(id));
        }
    }

    #[test]
    fn offline_retry_resets_delivery_evidence_and_discards_only_its_credit() {
        let (root, paths, event_id) = fixture("retry");
        let item_id = ItemId::new("song").unwrap();
        let mut before = load_store_set(&paths).unwrap().unwrap();
        before
            .bridge_state
            .upsert_aggregate_play_shadow(
                item_id.clone(),
                AggregatePlayShadow {
                    play_count: 0,
                    played_at: None,
                    observed_at_unix: 43,
                    counter_epoch: 1,
                },
            )
            .unwrap();
        before
            .bridge_state
            .reserve_outbound_exact_history_credit(item_id.clone(), 1)
            .unwrap();
        let expected = before.revisions();
        commit_store_set(&paths, expected, &mut before).unwrap();

        resolve_scrobble_attention(&paths, &event_id, OutboundScrobbleResolution::Retry).unwrap();

        let durable = load_store_set(&paths).unwrap().unwrap();
        let pending = durable.bridge_state.outbound_scrobbles().front().unwrap();
        assert_eq!(pending.delivery, OutboundScrobbleDelivery::Queued);
        assert!(!pending.baseline_captured);
        assert_eq!(pending.baseline_play_count, None);
        assert_eq!(pending.baseline_played_at, None);
        assert!(!pending.exact_credit_recorded);
        assert_eq!(pending.exact_credit_epoch, None);
        assert_eq!(pending.uncertain_readbacks, 0);
        let credits = durable
            .bridge_state
            .history_dedupe_credits()
            .get(&item_id)
            .unwrap();
        assert!(
            credits.get(&0).is_none(),
            "retry must discard the reserve from its saved counter epoch"
        );
        assert_eq!(credits.get(&1).unwrap().exact_unmatched, 1);
        let _ = std::fs::remove_dir_all(root);
    }
}
