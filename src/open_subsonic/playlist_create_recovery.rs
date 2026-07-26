//! Offline owner-store recovery for replay-unsafe server playlist creates.
//!
//! Listing never starts an actor or performs network I/O. Abandoning an intent is an explicit
//! user decision: the server may already contain a created copy, so callers must confirm before
//! invoking it and must already own the process-wide persistence writer lease.

use super::actor::ServiceError;
use super::bridge_runtime::BridgeRuntime;
use super::bridge_store::OpenSubsonicBridgeState;
use super::profile::OpenSubsonicPaths;
use super::transaction::{load_store_set, load_store_set_read_only};
use crate::personal_state::PlaylistId;

/// Redacted recovery evidence safe for status, CLI, and TUI surfaces.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaylistCreateRecoveryState {
    /// The create response was lost, so no remote identifier is known.
    ServerIdentityUnknown,
    /// A remote identifier is known, but exact owner/readback verification has not completed.
    ReadbackNeeded,
}

impl PlaylistCreateRecoveryState {
    pub const fn label(self) -> &'static str {
        match self {
            Self::ServerIdentityUnknown => "server_identity_unknown",
            Self::ReadbackNeeded => "readback_needed",
        }
    }
}

/// A local playlist identity plus the minimum evidence needed for an explicit recovery decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlaylistCreateAttention {
    pub local_playlist_id: PlaylistId,
    pub state: PlaylistCreateRecoveryState,
}

/// List pending create decisions from one coherent read-only snapshot.
pub fn list_playlist_create_attention(
    paths: &OpenSubsonicPaths,
) -> Result<Vec<PlaylistCreateAttention>, ServiceError> {
    let store_set = load_store_set_read_only(paths)?.ok_or(ServiceError::InvalidSetup)?;
    Ok(playlist_create_attention_from_state(
        &store_set.bridge_state,
    ))
}

pub(crate) fn playlist_create_attention_from_state(
    bridge_state: &OpenSubsonicBridgeState,
) -> Vec<PlaylistCreateAttention> {
    bridge_state
        .pending_playlist_creates()
        .values()
        .map(|pending| PlaylistCreateAttention {
            local_playlist_id: pending.local_playlist_id.clone(),
            state: if pending.created_server_playlist_id.is_some() {
                PlaylistCreateRecoveryState::ReadbackNeeded
            } else {
                PlaylistCreateRecoveryState::ServerIdentityUnknown
            },
        })
        .collect()
}

/// Forget one durable create intent without contacting or deleting anything on the server.
pub fn abandon_playlist_create_attention(
    paths: &OpenSubsonicPaths,
    local_playlist_id: &PlaylistId,
) -> Result<(), ServiceError> {
    let mut store_set = load_store_set(paths)?.ok_or(ServiceError::InvalidSetup)?;
    BridgeRuntime::writable(paths.clone(), None)
        .cancel_playlist_create(&mut store_set, local_playlist_id)
}

#[cfg(test)]
mod tests {
    use age::secrecy::SecretString;

    use super::*;
    use crate::open_subsonic::bridge_store::PendingPlaylistCreate;
    use crate::open_subsonic::{
        ConfiguredPrivateOrigin, OpenSubsonicBridgeState, OpenSubsonicPrivateState,
        OpenSubsonicProfile, OpenSubsonicStoreSet, ServerCredential, ServerPlaylistId,
        StoreRevisions, commit_store_set,
    };

    fn fixture(
        label: &str,
    ) -> (
        std::path::PathBuf,
        OpenSubsonicPaths,
        PlaylistId,
        PlaylistId,
    ) {
        let root = std::env::temp_dir().join(format!(
            "yututui-playlist-create-recovery-{label}-{}",
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
        let unknown = PlaylistId::new("local-unknown").unwrap();
        let known = PlaylistId::new("local-known").unwrap();
        for (local_playlist_id, created_server_playlist_id) in [
            (unknown.clone(), None),
            (
                known.clone(),
                Some(ServerPlaylistId::new("server-known").unwrap()),
            ),
        ] {
            bridge_state
                .queue_playlist_create(PendingPlaylistCreate {
                    local_playlist_id,
                    expected_missing_server_id: None,
                    created_server_playlist_id,
                    desired_name: "Private name sentinel".to_owned(),
                    ordered_entry_ids: Vec::new(),
                    ordered_item_ids: Vec::new(),
                    started_at_unix: 42,
                })
                .unwrap();
        }
        let mut store_set =
            OpenSubsonicStoreSet::new(profile, private_state, bridge_state).unwrap();
        commit_store_set(&paths, StoreRevisions::MISSING, &mut store_set).unwrap();
        (root, paths, unknown, known)
    }

    #[test]
    fn list_is_redacted_sorted_and_abandon_is_durable() {
        let (root, paths, unknown, known) = fixture("list-abandon");
        assert_eq!(
            list_playlist_create_attention(&paths).unwrap(),
            vec![
                PlaylistCreateAttention {
                    local_playlist_id: known.clone(),
                    state: PlaylistCreateRecoveryState::ReadbackNeeded,
                },
                PlaylistCreateAttention {
                    local_playlist_id: unknown,
                    state: PlaylistCreateRecoveryState::ServerIdentityUnknown,
                },
            ]
        );

        abandon_playlist_create_attention(&paths, &known).unwrap();
        let attention = list_playlist_create_attention(&paths).unwrap();
        assert_eq!(attention.len(), 1);
        assert_eq!(
            attention[0].state,
            PlaylistCreateRecoveryState::ServerIdentityUnknown
        );
        assert!(
            load_store_set(&paths)
                .unwrap()
                .unwrap()
                .bridge_state
                .pending_playlist_creates()
                .get(&known)
                .is_none()
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn abandon_rejects_an_identifier_that_is_not_pending() {
        let (root, paths, _, _) = fixture("missing");
        assert_eq!(
            abandon_playlist_create_attention(&paths, &PlaylistId::new("not-pending").unwrap()),
            Err(ServiceError::InvalidSetup)
        );
        let _ = std::fs::remove_dir_all(root);
    }
}
