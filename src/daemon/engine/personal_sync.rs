//! Owner-lane installation for detached manual-sync candidates.

use super::DaemonEngine;
use crate::personal_state::DeviceId;
use crate::sync::service::{AppliedManualSync, PreparedManualSync, SyncServiceError};

#[derive(Clone)]
pub(in crate::daemon) enum PersonalSyncAction {
    SyncNow,
    Revoke(DeviceId),
}

impl DaemonEngine {
    pub(in crate::daemon) fn set_personal_sync_in_progress(&mut self, in_progress: bool) {
        self.personal_sync_in_progress = in_progress;
    }

    pub(in crate::daemon) fn personal_sync_source(
        &mut self,
    ) -> Result<
        (
            crate::personal_state::PersonalStateV2,
            crate::sync::SyncPaths,
        ),
        SyncServiceError,
    > {
        if self.personal_state_device_id.is_none() {
            return Err(SyncServiceError::NotConfigured);
        }
        let state = self.persist_live_personal_state_for_sync()?;
        let personal_paths = self
            .personal_state_paths()
            .map_err(SyncServiceError::from)?;
        let sync_paths = crate::sync::SyncPaths::for_data_root(personal_paths.data_root.clone());
        Ok((state, sync_paths))
    }

    /// Reconcile and durably commit the current live projections before deciding whether a
    /// detached candidate still describes this owner generation.
    pub(in crate::daemon) fn personal_sync_source_is_current(
        &mut self,
        expected_revision: u64,
    ) -> Result<bool, SyncServiceError> {
        if self.personal_state_device_id.is_none() {
            return Err(SyncServiceError::NotConfigured);
        }
        Ok(self.persist_live_personal_state_for_sync()?.revision == expected_revision)
    }

    pub(in crate::daemon) fn apply_personal_sync_candidate(
        &mut self,
        action: PersonalSyncAction,
        candidate: PreparedManualSync,
    ) -> Result<AppliedManualSync, SyncServiceError> {
        let current_state = self.persist_live_personal_state_for_sync()?;
        if current_state.revision != candidate.expected_local_revision {
            return Err(SyncServiceError::LocalStateChanged);
        }
        let personal_paths = self
            .personal_state_paths()
            .map_err(SyncServiceError::from)?;
        let sync_paths = crate::sync::SyncPaths::for_data_root(personal_paths.data_root.clone());
        let applied = match action {
            PersonalSyncAction::SyncNow => crate::sync::service::apply_prepared_sync_now(
                &current_state,
                self.playlists.revision(),
                candidate,
                &personal_paths,
                &sync_paths,
            )?,
            PersonalSyncAction::Revoke(device_id) => {
                crate::sync::service::apply_prepared_revoke_now(
                    &current_state,
                    self.playlists.revision(),
                    &device_id,
                    candidate,
                    &personal_paths,
                    &sync_paths,
                )?
            }
        };
        self.install_personal_sync_runtime(&applied)?;
        Ok(applied)
    }

    #[cfg(test)]
    pub(in crate::daemon) fn personal_sync_in_progress_for_test(&self) -> bool {
        self.personal_sync_in_progress
    }

    #[cfg(test)]
    pub(in crate::daemon) fn configure_personal_sync_for_test(
        &mut self,
        state: crate::personal_state::PersonalStateV2,
        device_id: DeviceId,
    ) {
        let commit = crate::personal_state::PersonalStateCommit::prepare_for_runtime(
            state,
            self.playlists.revision(),
        )
        .expect("prepare daemon personal-sync fixture");
        let installed = commit
            .commit(&self.personal_state_paths)
            .expect("commit daemon personal-sync fixture");
        let (library, playlists, signals, station) = commit.runtime_stores();
        self.personal_state = installed;
        self.personal_state_device_id = Some(device_id);
        self.library = library;
        self.playlists = playlists;
        self.signals = signals;
        self.station = station;
    }

    fn install_personal_sync_runtime(
        &mut self,
        applied: &AppliedManualSync,
    ) -> Result<(), SyncServiceError> {
        let downloaded_operations = applied.summary.downloaded_operations;
        let prepared = crate::personal_state::PersonalStateCommit::prepare_for_runtime(
            applied.state.clone(),
            self.playlists.revision(),
        )
        .map_err(SyncServiceError::from)?;
        let (library, mut playlists, signals, station) = prepared.runtime_stores();
        let playlists_changed = playlists.inherit_revision_from(&self.playlists);
        self.personal_state = prepared.state().clone();
        self.library = library;
        self.playlists = playlists;
        self.signals = signals;
        self.station = station;
        if playlists_changed {
            self.bump_playlists_rev();
        }
        if downloaded_operations > 0 {
            self.library_invalidations = self.library_invalidations.wrapping_add(1);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn configured_engine() -> (DaemonEngine, DeviceId) {
        let mut engine = super::super::tests::engine_with_queue(&[]);
        let mut state = crate::personal_state::legacy_state(
            &crate::library::Library::default(),
            &crate::playlists::Playlists::default(),
            &crate::signals::Signals::default(),
            &crate::station::StationStore::default(),
        )
        .unwrap();
        let device_id = state
            .device_registry
            .values()
            .find(|device| device.device_id.as_str() != "legacy")
            .unwrap()
            .device_id
            .clone();
        let secrets = crate::sync::DeviceSecretMaterial::generate_for(device_id.as_str()).unwrap();
        state = crate::personal_state::append_operation_as(
            &state,
            &device_id,
            crate::personal_state::Operation::AddDevice {
                device: crate::personal_state::DeviceRecord {
                    device_id: device_id.clone(),
                    name: "Daemon".to_owned(),
                    revoked: false,
                    public_identity: Some(secrets.public_identity()),
                },
            },
            0,
        )
        .unwrap();
        let commit = crate::personal_state::PersonalStateCommit::prepare_for_runtime(
            state,
            engine.playlists.revision(),
        )
        .unwrap();
        let installed = commit.commit(&engine.personal_state_paths).unwrap();
        let (library, playlists, signals, station) = commit.runtime_stores();
        engine.personal_state = installed;
        engine.personal_state_device_id = Some(device_id.clone());
        engine.library = library;
        engine.playlists = playlists;
        engine.signals = signals;
        engine.station = station;
        (engine, device_id)
    }

    fn song(id: &str) -> crate::api::Song {
        crate::api::Song::remote(id, format!("Title {id}"), "Artist", "3:00")
    }

    fn detached_candidate(
        state: crate::personal_state::PersonalStateV2,
        device_id: DeviceId,
    ) -> PreparedManualSync {
        let recovery = crate::sync::RecoveryKit::generate(state.dataset_id.clone(), None).unwrap();
        let root = crate::sync::SignedMembershipRoot::create(
            state.dataset_id.clone(),
            recovery.recovery_recipient(),
            &recovery.signing_key().unwrap(),
            state.device_registry[&device_id].clone(),
        )
        .unwrap();
        PreparedManualSync {
            expected_local_revision: state.revision,
            expected_private_revision: 1,
            local_device_id: device_id,
            state,
            membership: crate::sync::MembershipChain::new(root),
            checkpoint_anchor: crate::sync::CheckpointAnchor::default(),
            summary: crate::sync::manual::ManualSyncSummary::default(),
        }
    }

    fn state_has_favorite(
        state: &crate::personal_state::PersonalStateV2,
        exact_catalog_id: &str,
    ) -> bool {
        crate::personal_state::project(state)
            .unwrap()
            .legacy
            .favorites
            .iter()
            .any(|track| {
                matches!(
                    &track.key,
                    crate::personal_state::PortableTrackKey::Catalog {
                        exact_catalog_id: id,
                        ..
                    } if id == exact_catalog_id
                )
            })
    }

    fn projected_state(
        library: &crate::library::Library,
        playlists: &crate::playlists::Playlists,
    ) -> crate::personal_state::PersonalStateV2 {
        crate::personal_state::legacy_state(
            library,
            playlists,
            &crate::signals::Signals::default(),
            &crate::station::StationStore::default(),
        )
        .unwrap()
    }

    fn applied(
        state: crate::personal_state::PersonalStateV2,
        downloaded_operations: usize,
    ) -> AppliedManualSync {
        AppliedManualSync {
            state,
            summary: crate::sync::manual::ManualSyncSummary {
                downloaded_operations,
                ..crate::sync::manual::ManualSyncSummary::default()
            },
        }
    }

    #[test]
    fn sync_source_durably_recovers_a_live_mutation_after_save_failure() {
        let (mut engine, device_id) = configured_engine();
        let before = engine.personal_state.clone();
        let favorite = song("failed-before-sync");
        engine.library.toggle_favorite(&favorite);
        {
            let _guard = super::super::persistence_gate::fail_store_saves_for_test(
                crate::persist::StoreKind::Library,
            );
            engine.save_library("injected pre-sync failure");
        }

        assert_eq!(engine.personal_state, before);
        assert!(engine.library.is_favorite(favorite.video_id.as_str()));

        let (source, _) = engine.personal_sync_source().unwrap();

        assert!(source.revision > before.revision);
        assert!(state_has_favorite(&source, "failed-before-sync"));
        assert_eq!(
            crate::personal_state::load_ledger(&engine.personal_state_paths)
                .unwrap()
                .as_ref(),
            Some(&source)
        );
        assert!(source.operations.iter().any(|operation| {
            operation.stamp.dot.device_id == device_id
                && matches!(
                    &operation.operation,
                    crate::personal_state::Operation::SetRating { track, .. }
                        if matches!(
                            &track.key,
                            crate::personal_state::PortableTrackKey::Catalog {
                                exact_catalog_id,
                                ..
                            } if exact_catalog_id == "failed-before-sync"
                        )
                )
        }));
    }

    #[test]
    fn sync_source_recovery_preflight_preserves_the_uncommitted_live_projection() {
        let (mut engine, _) = configured_engine();
        let before = engine.personal_state.clone();
        let favorite = song("recovery-preflight");
        engine.library.toggle_favorite(&favorite);
        let _guard = super::super::persistence_gate::fail_recovery_for_test(
            crate::persist::StartupRecoveryError {
                store: crate::persist::StoreKind::PersonalState,
                failure: crate::persist::StartupRecoveryFailure::LockFailure {
                    kind: std::io::ErrorKind::WouldBlock,
                    error: "injected sync recovery failure".to_owned(),
                },
            },
        );

        let error = engine.personal_sync_source().err().unwrap();

        assert_eq!(error, SyncServiceError::Storage);
        assert_eq!(engine.personal_state, before);
        assert!(engine.library.is_favorite(favorite.video_id.as_str()));
        assert_eq!(
            crate::personal_state::load_ledger(&engine.personal_state_paths)
                .unwrap()
                .as_ref(),
            Some(&before)
        );
    }

    #[test]
    fn apply_reconciles_failed_live_save_before_rejecting_stale_candidate() {
        let (mut engine, device_id) = configured_engine();
        let observed = engine.personal_state.clone();
        let candidate = detached_candidate(observed.clone(), device_id);
        let favorite = song("failed-during-sync");
        engine.library.toggle_favorite(&favorite);
        {
            let _guard = super::super::persistence_gate::fail_store_saves_for_test(
                crate::persist::StoreKind::Library,
            );
            engine.save_library("injected detached-sync failure");
        }
        assert_eq!(engine.personal_state, observed);

        let error = engine
            .apply_personal_sync_candidate(PersonalSyncAction::SyncNow, candidate)
            .err()
            .unwrap();

        assert_eq!(error, SyncServiceError::LocalStateChanged);
        assert!(engine.library.is_favorite(favorite.video_id.as_str()));
        assert!(state_has_favorite(
            &engine.personal_state,
            "failed-during-sync"
        ));
        assert_eq!(
            crate::personal_state::load_ledger(&engine.personal_state_paths)
                .unwrap()
                .as_ref(),
            Some(&engine.personal_state)
        );
    }

    #[test]
    fn stale_checks_commit_the_union_before_each_detached_retry() {
        let (mut engine, device_id) = configured_engine();
        let (initial_source, _) = engine.personal_sync_source().unwrap();
        let first = song("retry-union-a");
        engine.library.toggle_favorite(&first);
        {
            let _guard = super::super::persistence_gate::fail_store_saves_for_test(
                crate::persist::StoreKind::Library,
            );
            engine.save_library("first detached mutation");
        }

        assert!(
            !engine
                .personal_sync_source_is_current(initial_source.revision)
                .unwrap()
        );
        let first_retry = engine.personal_state.clone();
        assert!(state_has_favorite(&first_retry, "retry-union-a"));

        let second = song("retry-union-b");
        engine.library.toggle_favorite(&second);
        {
            let _guard = super::super::persistence_gate::fail_store_saves_for_test(
                crate::persist::StoreKind::Library,
            );
            engine.save_library("second detached mutation");
        }

        assert!(
            !engine
                .personal_sync_source_is_current(first_retry.revision)
                .unwrap()
        );
        assert!(state_has_favorite(&engine.personal_state, "retry-union-a"));
        assert!(state_has_favorite(&engine.personal_state, "retry-union-b"));
        assert!(
            engine
                .personal_state
                .operations
                .iter()
                .filter(|operation| {
                    matches!(
                        operation.operation,
                        crate::personal_state::Operation::SetRating { .. }
                    )
                })
                .all(|operation| operation.stamp.dot.device_id == device_id)
        );
        assert_eq!(
            crate::personal_state::load_ledger(&engine.personal_state_paths)
                .unwrap()
                .as_ref(),
            Some(&engine.personal_state)
        );
    }

    #[test]
    fn runtime_install_bumps_playlist_generations_only_for_playlist_content() {
        let mut engine = super::super::tests::engine_with_queue(&[]);
        let initial_store_revision = engine.playlists.revision();
        let initial_published_revision = engine.playlists_rev();
        let mut playlists = crate::playlists::Playlists::default();
        let playlist_id = playlists.create("Remote mix").unwrap();
        playlists.add(
            &playlist_id,
            crate::api::Song::remote("remote-track", "Remote track", "Artist", "3:00"),
        );
        let playlist_state = projected_state(&crate::library::Library::default(), &playlists);

        engine
            .install_personal_sync_runtime(&applied(playlist_state.clone(), 2))
            .unwrap();
        let store_revision = initial_store_revision.wrapping_add(1);
        let published_revision = initial_published_revision.wrapping_add(1);
        assert_eq!(engine.playlists.revision(), store_revision);
        assert_eq!(engine.playlists_rev(), published_revision);

        engine
            .install_personal_sync_runtime(&applied(playlist_state, 0))
            .unwrap();
        assert_eq!(
            engine.playlists.revision(),
            store_revision,
            "no-op sync must preserve the store generation"
        );
        assert_eq!(
            engine.playlists_rev(),
            published_revision,
            "no-op sync must preserve the published cache generation"
        );

        let mut liked_library = crate::library::Library::default();
        liked_library.favorites.push(crate::api::Song::remote(
            "liked-track",
            "Liked track",
            "Artist",
            "3:00",
        ));
        let rating_only_state = projected_state(&liked_library, &playlists);
        let invalidations_before_rating = engine.library_invalidations();
        engine
            .install_personal_sync_runtime(&applied(rating_only_state, 1))
            .unwrap();

        assert_eq!(engine.playlists.revision(), store_revision);
        assert_eq!(
            engine.playlists_rev(),
            published_revision,
            "rating-only sync must not publish a playlist cache generation"
        );
        assert_eq!(
            engine.library_invalidations(),
            invalidations_before_rating.wrapping_add(1),
            "the rating change still invalidates library consumers"
        );
    }
}
