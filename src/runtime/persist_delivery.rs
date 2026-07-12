use crate::app::{App, PersistCmd};
use crate::persist::{PersistHandle, Snapshot};
use crate::util::delivery::DeliveryResult;

pub(super) fn admit(handle: &PersistHandle, app: &App, command: PersistCmd) -> DeliveryResult {
    match command {
        PersistCmd::Library => handle.save(Snapshot::Library(app.library.clone())),
        PersistCmd::Downloads => handle.save(Snapshot::Downloads(app.download_store.clone())),
        PersistCmd::Signals => handle.save(Snapshot::Signals(app.signals.clone())),
        PersistCmd::RomanizedTitles => {
            handle.save(Snapshot::RomanizedTitles(app.romanization.cache.clone()))
        }
        PersistCmd::ClearRomanizedTitles => handle.delete_romanized_titles(),
        PersistCmd::Config(config) => handle.save(Snapshot::Config(config)),
        PersistCmd::Playlists => handle.save(Snapshot::Playlists(app.playlists.clone())),
        PersistCmd::StationProfile => handle.save(Snapshot::Station(app.station.clone())),
    }
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
        let app = App::new(50);

        assert_eq!(
            admit(&handle, &app, PersistCmd::Library),
            Err(crate::util::delivery::DeliveryError::Closed)
        );
    }
}
