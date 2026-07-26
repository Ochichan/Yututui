//! Owner-lane installation of durable OpenSubsonic observations.

use super::App;

impl App {
    /// Merge an exactly-once server observation into the canonical ledger and refresh all runtime
    /// projections. The runtime persists the resulting snapshot before acknowledging the bridge.
    pub(crate) fn apply_open_subsonic_bridge_import(
        &mut self,
        import: &crate::open_subsonic::OpenSubsonicBridgeImport,
    ) -> Result<String, crate::personal_state::PersonalStateError> {
        let current = self.reconcile_personal_state(&self.playlists)?;
        let operation = import.operation();
        let origin = import.origin()?;
        let (candidate, envelope_id) = match &self.personal_state.device_id {
            Some(device_id) => {
                let envelope_id = crate::personal_state::external_operation_envelope_id(
                    device_id,
                    import.operation_id(),
                )?;
                let candidate = crate::personal_state::append_external_operation_as(
                    &current,
                    device_id,
                    import.operation_id().to_owned(),
                    origin,
                    operation,
                    import.observed_at_unix(),
                )?;
                (candidate, envelope_id)
            }
            None => {
                let envelope_id = crate::personal_state::external_operation_envelope_id_for_state(
                    &current,
                    import.operation_id(),
                )?;
                let candidate = crate::personal_state::append_external_operation(
                    &current,
                    import.operation_id().to_owned(),
                    origin,
                    operation,
                    import.observed_at_unix(),
                )?;
                (candidate, envelope_id)
            }
        };
        self.install_personal_state_runtime(candidate)?;
        Ok(envelope_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::open_subsonic::OpenSubsonicBridgeImport;
    use crate::personal_state::{PortableTrack, PortableTrackKey, Rating};

    fn portable() -> PortableTrack {
        PortableTrack {
            key: PortableTrackKey::OpenSubsonic {
                backend_id: "backend".to_owned(),
                account_scope_id: "account".to_owned(),
                item_id: "song".to_owned(),
            },
            title: "Server title".to_owned(),
            artist: "Server artist".to_owned(),
            album: Some("Server album".to_owned()),
            duration_secs: Some(180),
            isrc: None,
        }
    }

    fn app_with_legacy_state() -> App {
        let mut app = App::new(50);
        let state = crate::personal_state::legacy_state(
            &app.library,
            &app.playlists,
            &app.signals,
            &app.station,
        )
        .unwrap();
        app.install_personal_state_runtime(state).unwrap();
        app
    }

    #[test]
    fn unsynced_owner_imports_and_replays_one_server_rating_exactly_once() {
        let mut app = app_with_legacy_state();
        let import = OpenSubsonicBridgeImport::Rating {
            operation_id: "server-rating-observation".to_owned(),
            track: portable(),
            rating: Rating::Liked,
            observed_at_unix: 100,
        };

        let envelope_id = app.apply_open_subsonic_bridge_import(&import).unwrap();
        assert_eq!(
            app.apply_open_subsonic_bridge_import(&import).unwrap(),
            envelope_id
        );

        assert_eq!(
            app.personal_state
                .ledger
                .operations
                .iter()
                .filter(|operation| operation.operation_id == envelope_id)
                .count(),
            1
        );
        assert_ne!(envelope_id, import.operation_id());
        assert_eq!(app.library.favorites.len(), 1);
        assert_eq!(app.library.favorites[0].title, "Server title");
    }
}
