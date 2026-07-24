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
    }
}

fn admit_personal_state(handle: &PersistHandle, app: &mut App) -> DeliveryResult {
    let candidate = crate::personal_state::reconcile_runtime(
        &app.personal_state,
        &app.library,
        &app.playlists,
        &app.signals,
        &app.station,
    )
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
    app.personal_state = commit.state().clone();
    app.library = std::sync::Arc::new(library);
    app.playlists = std::sync::Arc::new(playlists);
    app.signals = std::sync::Arc::new(signals);
    app.station = station;
    handle.save(Snapshot::PersonalState(Box::new(commit)))
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
