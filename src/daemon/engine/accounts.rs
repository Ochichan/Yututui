//! The `accounts` topic projection and its config-side mutations (C5).
//!
//! Secrets never leave this module's inputs: the projection carries presence booleans
//! and display names only; `account_set` accepts no credential fields (the Last.fm
//! session key arrives through the scrobble actor's auth flow, the ListenBrainz token
//! through `listen_brainz_configure`, and both are write-only).

use crate::remote::proto::{
    LastfmAccountModel, ListenBrainzAccountModel, RemoteResponse, SpotifyAccountModel,
};

use super::DaemonEngine;

/// The retained `accounts` snapshot pieces, in push order.
pub(crate) struct AccountsModels {
    pub lastfm: LastfmAccountModel,
    pub listenbrainz: ListenBrainzAccountModel,
    pub spotify: SpotifyAccountModel,
    pub scrobble_local: bool,
}

impl DaemonEngine {
    pub fn accounts_rev(&self) -> u64 {
        self.accounts_rev
    }

    pub(in crate::daemon) fn bump_accounts_rev(&mut self) {
        self.accounts_rev = self.accounts_rev.wrapping_add(1);
    }

    pub(crate) fn accounts_models(&self) -> AccountsModels {
        let lastfm = &self.config.scrobble.lastfm;
        let listenbrainz = &self.config.scrobble.listenbrainz;
        let lastfm_connected = lastfm
            .session_key
            .as_deref()
            .is_some_and(|key| !key.is_empty());
        AccountsModels {
            lastfm: LastfmAccountModel {
                connected: lastfm_connected,
                user: lastfm.username.clone().filter(|_| lastfm_connected),
                scrobbling: lastfm.is_active(),
                love_sync: lastfm.love_sync.unwrap_or(true),
            },
            listenbrainz: ListenBrainzAccountModel {
                submit: listenbrainz.is_active(),
                has_token: listenbrainz
                    .token
                    .as_deref()
                    .is_some_and(|token| !token.is_empty()),
                custom_url: listenbrainz.api_url.clone(),
            },
            spotify: SpotifyAccountModel {
                connected: self.spotify_user.is_some() || spotify_token_present(),
                user: self.spotify_user.clone(),
                client_id: self.config.spotify.client_id.clone(),
                redirect_port: Some(self.config.effective_spotify_port()),
            },
            scrobble_local: self.config.effective_scrobble_local_files(),
        }
    }

    /// The Last.fm auth flow completed (scrobble actor `AuthDone`): persist the session,
    /// bump the topic. The caller reconfigures the live scrobble actor.
    pub(in crate::daemon) fn apply_lastfm_session(
        &mut self,
        username: String,
        session_key: String,
    ) {
        self.config.scrobble.lastfm.session_key = Some(session_key);
        self.config.scrobble.lastfm.username = Some(username);
        self.save_config("daemon lastfm auth");
        self.bump_accounts_rev();
    }

    /// The Spotify PKCE flow completed / disconnected (transfer actor events).
    pub(in crate::daemon) fn set_spotify_user(&mut self, user: Option<String>) {
        if self.spotify_user != user {
            self.spotify_user = user;
            self.bump_accounts_rev();
        }
    }

    pub(in crate::daemon) fn spotify_auth_config(&self) -> (Option<String>, u16) {
        (
            self.config.spotify.client_id.clone(),
            self.config.effective_spotify_port(),
        )
    }

    pub(super) fn gui_listen_brainz_configure(
        &mut self,
        submit: Option<bool>,
        token: Option<String>,
        custom_url: Option<String>,
    ) -> RemoteResponse {
        let listenbrainz = &mut self.config.scrobble.listenbrainz;
        if let Some(submit) = submit {
            listenbrainz.enabled = Some(submit);
        }
        if let Some(token) = token {
            // Write-only credential; an empty string disconnects.
            let token = token.trim().to_owned();
            listenbrainz.token = (!token.is_empty()).then_some(token);
        }
        if let Some(url) = custom_url {
            let url = url.trim().to_owned();
            listenbrainz.api_url = (!url.is_empty()).then_some(url);
        }
        self.save_config("daemon listenbrainz configure");
        self.bump_accounts_rev();
        RemoteResponse::ok("listenbrainz configured".to_owned())
    }

    /// The GUI's uniform non-credential account field setter. Unknown service/field
    /// pairs answer `unknown_setting` so a frontend typo cannot silently no-op.
    pub(super) fn gui_account_set(
        &mut self,
        service: &str,
        field: &str,
        value: &serde_json::Value,
    ) -> RemoteResponse {
        let applied = match (service, field) {
            ("lastfm", "scrobbling") => value.as_bool().map(|on| {
                self.config.scrobble.lastfm.enabled = Some(on);
            }),
            ("lastfm", "love_sync") => value.as_bool().map(|on| {
                self.config.scrobble.lastfm.love_sync = Some(on);
            }),
            ("lastfm", "scrobble_local") => value.as_bool().map(|on| {
                self.config.scrobble.local_files = Some(on);
            }),
            ("listenbrainz", "submit") => value.as_bool().map(|on| {
                self.config.scrobble.listenbrainz.enabled = Some(on);
            }),
            ("spotify", "client_id") => value.as_str().map(|id| {
                let id = id.trim().to_owned();
                self.config.spotify.client_id = (!id.is_empty()).then_some(id);
            }),
            ("spotify", "redirect_port") => value
                .as_u64()
                .and_then(|port| u16::try_from(port).ok())
                .map(|port| {
                    self.config.spotify.redirect_port = Some(port);
                }),
            _ => None,
        };
        if applied.is_none() {
            return RemoteResponse::err("unknown_setting");
        }
        self.save_config("daemon account set");
        self.bump_accounts_rev();
        RemoteResponse::ok("account updated".to_owned())
    }
}

/// Whether a persisted Spotify token exists (presence only; the token is never read
/// here). Answers "connected" across daemon restarts until the transfer actor reports
/// a live display name.
fn spotify_token_present() -> bool {
    crate::spotify::auth::token_path().is_some_and(|path| path.exists())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    #[test]
    fn account_set_covers_the_gui_fields_and_rejects_unknowns() {
        let mut engine = super::super::tests::engine_with_queue(&[]);
        engine.remote_persistence_command_active = true;
        engine.remote_persistence_read_only = true; // saves become no-ops in tests

        let rev = engine.accounts_rev();
        assert!(
            engine
                .gui_account_set("lastfm", "scrobbling", &json!(false))
                .ok
        );
        assert_eq!(engine.config.scrobble.lastfm.enabled, Some(false));
        assert!(
            engine
                .gui_account_set("lastfm", "love_sync", &json!(false))
                .ok
        );
        assert!(
            engine
                .gui_account_set("lastfm", "scrobble_local", &json!(false))
                .ok
        );
        assert!(
            engine
                .gui_account_set("listenbrainz", "submit", &json!(true))
                .ok
        );
        assert!(
            engine
                .gui_account_set("spotify", "client_id", &json!("abc123"))
                .ok
        );
        assert!(
            engine
                .gui_account_set("spotify", "redirect_port", &json!(9700))
                .ok
        );
        assert_eq!(engine.config.spotify.redirect_port, Some(9700));
        assert!(engine.accounts_rev() > rev);

        let unknown = engine.gui_account_set("lastfm", "nonsense", &json!(true));
        assert_eq!(unknown.reason.as_deref(), Some("unknown_setting"));
        let wrong_type = engine.gui_account_set("lastfm", "scrobbling", &json!("yes"));
        assert_eq!(wrong_type.reason.as_deref(), Some("unknown_setting"));
    }

    #[test]
    fn listenbrainz_configure_is_write_only_and_projection_hides_the_token() {
        let mut engine = super::super::tests::engine_with_queue(&[]);
        engine.remote_persistence_command_active = true;
        engine.remote_persistence_read_only = true;

        assert!(
            engine
                .gui_listen_brainz_configure(
                    Some(true),
                    Some("secret-token".to_owned()),
                    Some("https://lb.example".to_owned()),
                )
                .ok
        );
        let models = engine.accounts_models();
        assert!(models.listenbrainz.submit);
        assert!(models.listenbrainz.has_token);
        assert_eq!(
            models.listenbrainz.custom_url.as_deref(),
            Some("https://lb.example")
        );
        // The projection never carries the token itself.
        let serialized =
            serde_json::to_string(&crate::remote::proto::PushEvent::AccountsSnapshot {
                lastfm: models.lastfm,
                listenbrainz: models.listenbrainz,
                spotify: models.spotify,
                scrobble_local: models.scrobble_local,
            })
            .unwrap();
        assert!(!serialized.contains("secret-token"));

        // Empty token disconnects.
        assert!(
            engine
                .gui_listen_brainz_configure(None, Some(String::new()), None)
                .ok
        );
        assert!(!engine.accounts_models().listenbrainz.has_token);
    }

    #[test]
    fn lastfm_session_apply_projects_presence_not_the_key() {
        let mut engine = super::super::tests::engine_with_queue(&[]);
        engine.remote_persistence_command_active = true;
        engine.remote_persistence_read_only = true;

        engine.apply_lastfm_session("ochi".to_owned(), "session-secret".to_owned());
        let models = engine.accounts_models();
        assert!(models.lastfm.connected);
        assert_eq!(models.lastfm.user.as_deref(), Some("ochi"));
        let serialized =
            serde_json::to_string(&crate::remote::proto::PushEvent::AccountsSnapshot {
                lastfm: models.lastfm,
                listenbrainz: models.listenbrainz,
                spotify: models.spotify,
                scrobble_local: models.scrobble_local,
            })
            .unwrap();
        assert!(!serialized.contains("session-secret"));
    }
}
