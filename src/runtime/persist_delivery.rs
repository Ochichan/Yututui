use crate::app::{App, PersistCmd};
use crate::persist::{PersistHandle, Snapshot};
use crate::util::delivery::DeliveryResult;

pub(super) fn admit(handle: &PersistHandle, app: &mut App, command: PersistCmd) -> DeliveryResult {
    match command {
        PersistCmd::Library
        | PersistCmd::Signals
        | PersistCmd::Playlists
        | PersistCmd::StationProfile => admit_personal_state(handle, app),
        PersistCmd::Downloads => handle.save(Snapshot::Downloads(app.download_store.clone())),
        PersistCmd::RomanizedTitles => {
            handle.save(Snapshot::RomanizedTitles(app.romanization.cache.clone()))
        }
        PersistCmd::ClearRomanizedTitles => handle.delete_romanized_titles(),
        PersistCmd::Config(config) => handle.save(Snapshot::Config(config)),
        PersistCmd::TransferPlaylistCommit(_) => {
            unreachable!("targeted transfer persistence is dispatched before snapshot markers")
        }
        PersistCmd::PersonalSyncCommit(_) => {
            unreachable!("targeted sync persistence is dispatched before snapshot markers")
        }
    }
}

fn admit_personal_state(handle: &PersistHandle, app: &mut App) -> DeliveryResult {
    let candidate = app
        .reconcile_personal_state(&app.playlists)
        .and_then(|state| {
            crate::personal_state::PersonalStateCommit::prepare_for_runtime(
                state,
                app.playlists.revision(),
            )
        });
    let commit = match candidate {
        Ok(commit) => commit,
        Err(error) => {
            tracing::error!(%error, "personal-state commit preparation failed");
            return Err(crate::util::delivery::DeliveryError::Closed);
        }
    };
    let (library, playlists, signals, station) = commit.runtime_stores();
    app.personal_state.ledger = commit.state().clone();
    app.library = std::sync::Arc::new(library);
    app.playlists = std::sync::Arc::new(playlists);
    app.signals = std::sync::Arc::new(signals);
    app.station = station;
    handle.save(Snapshot::PersonalState(Box::new(commit)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn synced_state() -> (
        crate::personal_state::PersonalStateV2,
        crate::personal_state::DeviceId,
    ) {
        use crate::personal_state::{
            CausalStamp, DeviceId, DeviceRecord, Dot, Operation, OperationEnvelope,
            OperationOrigin, VersionVector,
        };

        let mut state =
            crate::personal_state::PersonalStateV2::empty("runtime-sync-test".to_owned()).unwrap();
        let author = DeviceId::new("membership").unwrap();
        let mut local_device = None;
        for (index, raw_device_id) in ["device-a", "device-b"].into_iter().enumerate() {
            let secrets = crate::sync::DeviceSecretMaterial::generate_for(raw_device_id).unwrap();
            let device_id = DeviceId::new(raw_device_id).unwrap();
            let dot = Dot {
                device_id: author.clone(),
                sequence: index as u64 + 1,
            };
            state.operations.push(OperationEnvelope {
                operation_id: format!("add-{raw_device_id}"),
                stamp: CausalStamp {
                    dot: dot.clone(),
                    observed: VersionVector::default(),
                    recorded_at_unix: 0,
                },
                origin: OperationOrigin::Local,
                operation: Operation::AddDevice {
                    device: DeviceRecord {
                        device_id: device_id.clone(),
                        name: raw_device_id.to_owned(),
                        revoked: false,
                        public_identity: Some(secrets.public_identity()),
                    },
                },
            });
            state.version_vector.observe(&dot);
            local_device.get_or_insert(device_id);
        }
        crate::personal_state::refresh_device_registry(&mut state).unwrap();
        state.normalize().unwrap();
        (state, local_device.unwrap())
    }

    #[tokio::test]
    async fn sealed_persistence_returns_closed_to_runtime_dispatch() {
        let handle = crate::persist::spawn();
        handle
            .seal_with_snapshots([])
            .expect("test shutdown seal precedes panic sealing");
        let mut app = App::new(50);

        assert_eq!(
            admit(&handle, &mut app, PersistCmd::Library),
            Err(crate::util::delivery::DeliveryError::Closed)
        );
    }

    #[tokio::test]
    async fn synced_runtime_persistence_authors_changes_as_the_bound_device() {
        let handle = crate::persist::spawn();
        let mut app = App::new(50);
        let (state, device_id) = synced_state();
        app.personal_state.ledger = state;
        app.personal_state.device_id = Some(device_id.clone());
        app.library_mut().toggle_favorite(&crate::api::Song::remote(
            "bound-rating",
            "Bound rating",
            "Artist",
            "3:00",
        ));

        let _ =
            admit(&handle, &mut app, PersistCmd::Library).expect("personal-state save is admitted");
        let rating = app
            .personal_state
            .ledger
            .operations
            .iter()
            .find(|operation| {
                matches!(
                    operation.operation,
                    crate::personal_state::Operation::SetRating { .. }
                )
            })
            .expect("rating operation");
        assert_eq!(rating.stamp.dot.device_id, device_id);

        handle
            .seal_with_snapshots([])
            .expect("seal test persistence");
    }
}
