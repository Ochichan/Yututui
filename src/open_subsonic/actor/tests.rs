use std::sync::atomic::{AtomicU64, Ordering};

use age::secrecy::SecretString;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

use super::playlist_catalog::merge_missing_playlist_recovery_rows;
use super::*;
use crate::open_subsonic::bridge_store::{
    PendingPlaylistProjection, PendingPlaylistProjectionStage, PlaylistLink, PlaylistLinkState,
    PlaylistShadow, RatingShadow,
};
use crate::open_subsonic::rating::RawServerRating;
use crate::open_subsonic::{
    ServerLibraryRow, ServerPlaylistAccess, ServerPlaylistLinkHealth, ServerPlaylistSummary,
};

static NEXT_ROOT: AtomicU64 = AtomicU64::new(0);
static LIFECYCLE_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[test]
fn status_never_contains_origin_or_credentials() {
    let status = OpenSubsonicStatus {
        kind: OpenSubsonicStatusKind::UpToDate,
        display_name: Some("Server".to_owned()),
        backend_id: Some(BackendId::new("backend").unwrap()),
        account_scope_id: Some(AccountScopeId::new("account").unwrap()),
        credential_kind: Some(CredentialKind::ApiKey),
        uses_lan_http: false,
        uses_custom_ca: false,
        native_history_enabled: false,
        native_history_health: NativeHistoryHealth::Off,
        outbound_scrobbles_needing_attention: 0,
        playlist_creates_needing_attention: 0,
        playlist_create_attention: Vec::new(),
        playlist_links_needing_decision: 0,
        playlist_projections_needing_attention: 0,
        playlist_contents_needing_attention: 0,
    };
    let rendered = format!("{status:?}");
    assert!(!rendered.contains("https://"));
    assert!(!rendered.contains("sentinel-secret"));
}

fn playlist_summary_for_access(
    owner: Option<&str>,
    owner_evidence: Option<&str>,
    readonly_evidence: Option<bool>,
) -> ServerPlaylistSummary {
    ServerPlaylistSummary {
        id: ServerPlaylistId::new("playlist").unwrap(),
        name: "Playlist".to_owned(),
        owner: owner.map(str::to_owned),
        song_count: Some(0),
        duration_secs: None,
        public: None,
        cover_art_id: None,
        access: ServerPlaylistAccess::ReadOnly,
        link: None,
        readonly_evidence,
        owner_evidence: owner_evidence.map(str::to_owned),
    }
}

fn empty_playlist_bridge_state() -> OpenSubsonicBridgeState {
    OpenSubsonicBridgeState::new(
        BackendId::new("backend").unwrap(),
        AccountScopeId::new("account").unwrap(),
    )
}

#[test]
fn actor_exposes_server_access_only_for_exact_credential_owner() {
    let password =
        ServerCredential::password("alice", SecretString::from("server-password".to_owned()))
            .unwrap();
    let mut page = ServerLibraryPage {
        section: ServerLibrarySection::Playlists,
        rows: vec![
            ServerLibraryRow::Playlist(playlist_summary_for_access(
                Some("alice"),
                Some("alice"),
                Some(false),
            )),
            ServerLibraryRow::Playlist(playlist_summary_for_access(
                Some("bob"),
                Some("bob"),
                Some(false),
            )),
            ServerLibraryRow::Playlist(playlist_summary_for_access(
                Some("alice"),
                Some("alice"),
                None,
            )),
            ServerLibraryRow::Playlist(playlist_summary_for_access(
                Some("alice"),
                None,
                Some(false),
            )),
        ],
        next_offset: None,
        warning: None,
    };
    finalize_page_playlist_access(&mut page, &password, &empty_playlist_bridge_state());
    let accesses = page
        .rows
        .iter()
        .map(|row| match row {
            ServerLibraryRow::Playlist(summary) => summary.access,
            _ => unreachable!(),
        })
        .collect::<Vec<_>>();
    assert_eq!(
        accesses,
        [
            ServerPlaylistAccess::Server,
            ServerPlaylistAccess::ReadOnly,
            ServerPlaylistAccess::ReadOnly,
            ServerPlaylistAccess::ReadOnly,
        ]
    );

    let api_key =
        ServerCredential::api_key(SecretString::from("server-api-key".to_owned())).unwrap();
    finalize_page_playlist_access(&mut page, &api_key, &empty_playlist_bridge_state());
    assert!(page.rows.iter().all(|row| matches!(
        row,
        ServerLibraryRow::Playlist(ServerPlaylistSummary {
            access: ServerPlaylistAccess::ReadOnly,
            ..
        })
    )));

    let mut bound_api_key =
        ServerCredential::api_key(SecretString::from("server-api-key".to_owned())).unwrap();
    bound_api_key.bind_api_key_username("alice").unwrap();
    finalize_page_playlist_access(&mut page, &bound_api_key, &empty_playlist_bridge_state());
    assert!(matches!(
        &page.rows[0],
        ServerLibraryRow::Playlist(ServerPlaylistSummary {
            access: ServerPlaylistAccess::Server,
            ..
        })
    ));
    assert!(page.rows[1..].iter().all(|row| matches!(
        row,
        ServerLibraryRow::Playlist(ServerPlaylistSummary {
            access: ServerPlaylistAccess::ReadOnly,
            ..
        })
    )));
}

#[test]
fn actor_applies_the_same_access_rule_to_playlist_detail() {
    let password =
        ServerCredential::password("alice", SecretString::from("server-password".to_owned()))
            .unwrap();
    let mut detail = ServerLibraryDetail::PlaylistEntries(crate::open_subsonic::ServerPlaylist {
        summary: playlist_summary_for_access(Some("alice"), Some("alice"), Some(false)),
        entries: Vec::new(),
    });
    finalize_detail_playlist_access(&mut detail, &password, &empty_playlist_bridge_state());
    assert!(matches!(
        detail,
        ServerLibraryDetail::PlaylistEntries(crate::open_subsonic::ServerPlaylist {
            summary: ServerPlaylistSummary {
                access: ServerPlaylistAccess::Server,
                ..
            },
            ..
        })
    ));
}

#[test]
fn actor_marks_linked_rows_and_surfaces_missing_server_recovery_rows() {
    let password =
        ServerCredential::password("alice", SecretString::from("server-password".to_owned()))
            .unwrap();
    let mut bridge_state = empty_playlist_bridge_state();
    bridge_state
        .upsert_playlist_link(PlaylistLink {
            local_playlist_id: crate::personal_state::PlaylistId::new("local").unwrap(),
            server_playlist_id: ServerPlaylistId::new("missing-server").unwrap(),
            managed_by_yututui: true,
            state: PlaylistLinkState::ServerMissing,
            content_needs_attention: false,
            shadow: PlaylistShadow {
                name: "Keep me".to_owned(),
                occurrences: Vec::new(),
                verified_at_unix: 100,
            },
        })
        .unwrap();
    let mut page = ServerLibraryPage {
        section: ServerLibrarySection::Playlists,
        rows: Vec::new(),
        next_offset: None,
        warning: None,
    };

    finalize_page_playlist_access(&mut page, &password, &bridge_state);
    merge_missing_playlist_recovery_rows(&mut page, &bridge_state, 0, 50);

    let ServerLibraryRow::Playlist(summary) = &page.rows[0] else {
        panic!("missing playlist recovery row");
    };
    assert_eq!(summary.id.as_str(), "missing-server");
    assert_eq!(summary.access, ServerPlaylistAccess::Linked);
    assert_eq!(
        summary.link.as_ref().map(|link| link.health),
        Some(ServerPlaylistLinkHealth::ServerMissing)
    );
    assert_eq!(
        summary
            .link
            .as_ref()
            .map(|link| link.local_playlist_id.as_str()),
        Some("local")
    );
}

#[test]
fn actor_keeps_server_missing_recovery_action_when_content_also_needs_attention() {
    let password =
        ServerCredential::password("alice", SecretString::from("server-password".to_owned()))
            .unwrap();
    let mut bridge_state = empty_playlist_bridge_state();
    bridge_state
        .upsert_playlist_link(PlaylistLink {
            local_playlist_id: crate::personal_state::PlaylistId::new("local").unwrap(),
            server_playlist_id: ServerPlaylistId::new("playlist").unwrap(),
            managed_by_yututui: true,
            state: PlaylistLinkState::ServerMissing,
            content_needs_attention: true,
            shadow: PlaylistShadow {
                name: "Keep me".to_owned(),
                occurrences: Vec::new(),
                verified_at_unix: 100,
            },
        })
        .unwrap();
    let mut page = ServerLibraryPage {
        section: ServerLibrarySection::Playlists,
        rows: vec![ServerLibraryRow::Playlist(playlist_summary_for_access(
            Some("alice"),
            Some("alice"),
            Some(false),
        ))],
        next_offset: None,
        warning: None,
    };

    finalize_page_playlist_access(&mut page, &password, &bridge_state);

    let ServerLibraryRow::Playlist(summary) = &page.rows[0] else {
        panic!("playlist row");
    };
    assert_eq!(
        summary.link.as_ref().map(|link| link.health),
        Some(ServerPlaylistLinkHealth::ServerMissing),
        "the actionable restore/unlink recovery state must not be hidden by content attention"
    );
}

#[test]
fn actor_surfaces_a_dormant_projection_as_link_attention() {
    let password =
        ServerCredential::password("alice", SecretString::from("server-password".to_owned()))
            .unwrap();
    let mut bridge_state = empty_playlist_bridge_state();
    let local_playlist_id = crate::personal_state::PlaylistId::new("local").unwrap();
    bridge_state
        .upsert_playlist_link(PlaylistLink {
            local_playlist_id: local_playlist_id.clone(),
            server_playlist_id: ServerPlaylistId::new("playlist").unwrap(),
            managed_by_yututui: true,
            state: PlaylistLinkState::Linked,
            content_needs_attention: false,
            shadow: PlaylistShadow {
                name: "Playlist".to_owned(),
                occurrences: Vec::new(),
                verified_at_unix: 100,
            },
        })
        .unwrap();
    bridge_state
        .queue_playlist_projection(
            local_playlist_id,
            PendingPlaylistProjection {
                desired_name: "Local edit".to_owned(),
                ordered_entry_ids: Vec::new(),
                ordered_item_ids: Vec::new(),
                stage: PendingPlaylistProjectionStage::NeedsAttention,
                base_remote_fingerprint: "fingerprint".to_owned(),
            },
        )
        .unwrap();
    let mut page = ServerLibraryPage {
        section: ServerLibrarySection::Playlists,
        rows: vec![ServerLibraryRow::Playlist(playlist_summary_for_access(
            Some("alice"),
            Some("alice"),
            Some(false),
        ))],
        next_offset: None,
        warning: None,
    };

    finalize_page_playlist_access(&mut page, &password, &bridge_state);

    let ServerLibraryRow::Playlist(summary) = &page.rows[0] else {
        panic!("playlist row");
    };
    assert_eq!(summary.access, ServerPlaylistAccess::Linked);
    assert_eq!(
        summary.link.as_ref().map(|link| link.health),
        Some(ServerPlaylistLinkHealth::NeedsAttention)
    );
}

#[test]
fn actor_surfaces_current_and_durable_playlist_access_attention() {
    let password =
        ServerCredential::password("alice", SecretString::from("server-password".to_owned()))
            .unwrap();
    let mut bridge_state = empty_playlist_bridge_state();
    let local_playlist_id = crate::personal_state::PlaylistId::new("local").unwrap();
    let mut link = PlaylistLink {
        local_playlist_id,
        server_playlist_id: ServerPlaylistId::new("playlist").unwrap(),
        managed_by_yututui: true,
        state: PlaylistLinkState::Linked,
        content_needs_attention: false,
        shadow: PlaylistShadow {
            name: "Playlist".to_owned(),
            occurrences: Vec::new(),
            verified_at_unix: 100,
        },
    };
    bridge_state.upsert_playlist_link(link.clone()).unwrap();

    for (owner, read_only) in [("mallory", Some(false)), ("alice", None)] {
        let mut page = ServerLibraryPage {
            section: ServerLibrarySection::Playlists,
            rows: vec![ServerLibraryRow::Playlist(playlist_summary_for_access(
                Some(owner),
                Some(owner),
                read_only,
            ))],
            next_offset: None,
            warning: None,
        };

        finalize_page_playlist_access(&mut page, &password, &bridge_state);

        let ServerLibraryRow::Playlist(summary) = &page.rows[0] else {
            panic!("playlist row");
        };
        assert_eq!(
            summary.link.as_ref().map(|link| link.health),
            Some(ServerPlaylistLinkHealth::NeedsAttention)
        );
    }

    link.state = PlaylistLinkState::AccessNeedsAttention;
    bridge_state.upsert_playlist_link(link).unwrap();
    let mut page = ServerLibraryPage {
        section: ServerLibrarySection::Playlists,
        rows: vec![ServerLibraryRow::Playlist(playlist_summary_for_access(
            Some("alice"),
            Some("alice"),
            Some(false),
        ))],
        next_offset: None,
        warning: None,
    };

    finalize_page_playlist_access(&mut page, &password, &bridge_state);

    let ServerLibraryRow::Playlist(summary) = &page.rows[0] else {
        panic!("playlist row");
    };
    assert_eq!(
        summary.link.as_ref().map(|link| link.health),
        Some(ServerPlaylistLinkHealth::NeedsAttention)
    );
}

#[test]
fn only_transient_native_history_failures_use_the_short_retry_window() {
    use crate::open_subsonic::NativeHistoryError;

    assert!(native_history_error_retries_soon(
        NativeHistoryError::Offline
    ));
    assert!(native_history_error_retries_soon(
        NativeHistoryError::TemporarilyUnavailable
    ));
    for error in [
        NativeHistoryError::InvalidCredential,
        NativeHistoryError::AuthenticationRequired,
        NativeHistoryError::PermissionDenied,
        NativeHistoryError::UnsupportedFeature,
        NativeHistoryError::InvalidResponse,
        NativeHistoryError::ResponseTooLarge,
    ] {
        assert!(!native_history_error_retries_soon(error));
    }
    assert_eq!(
        native_history_health_after(false, None),
        NativeHistoryHealth::Off
    );
    assert_eq!(
        native_history_health_after(true, None),
        NativeHistoryHealth::Detailed
    );
    assert_eq!(
        native_history_health_after(true, Some(NativeHistoryError::UnsupportedFeature)),
        NativeHistoryHealth::PlayCountsOnly
    );
    assert_eq!(
        native_history_health_after(true, Some(NativeHistoryError::InvalidCredential)),
        NativeHistoryHealth::UpdatePassword
    );
    assert_eq!(
        native_history_health_after(true, Some(NativeHistoryError::Offline)),
        NativeHistoryHealth::Probing
    );
}

#[test]
fn disabling_native_history_removes_only_its_credential() {
    let id = NEXT_ROOT.fetch_add(1, Ordering::Relaxed);
    let root = std::env::temp_dir().join(format!(
        "yututui-open-subsonic-disable-history-{}-{id}",
        std::process::id()
    ));
    let paths = OpenSubsonicPaths::for_data_root(root.clone());
    let profile = OpenSubsonicProfile::new(
        "History server",
        ConfiguredPrivateOrigin::new("http://127.0.0.1:4533/", true).unwrap(),
        None,
    )
    .unwrap();
    let mut private_state = OpenSubsonicPrivateState::new(
        profile.backend_id().clone(),
        profile.account_scope_id().clone(),
        ServerCredential::password("alice", SecretString::from("server-password".to_owned()))
            .unwrap(),
    );
    private_state
        .enable_native_history_reusing_server_password()
        .unwrap();
    let mut bridge_state = OpenSubsonicBridgeState::new(
        profile.backend_id().clone(),
        profile.account_scope_id().clone(),
    );
    bridge_state.set_native_history_health(NativeHistoryHealth::Detailed);
    let mut store_set = OpenSubsonicStoreSet::new(profile, private_state, bridge_state).unwrap();
    commit_store_set(&paths, StoreRevisions::MISSING, &mut store_set).unwrap();

    let status = disable_native_history(&paths).unwrap();
    assert!(!status.native_history_enabled);
    assert_eq!(status.native_history_health, NativeHistoryHealth::Off);
    let stored = load_store_set(&paths).unwrap().unwrap();
    assert_eq!(
        stored.private_state.credential_kind(),
        CredentialKind::Password
    );
    assert!(!stored.private_state.native_history_enabled());
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn confirmed_remove_resets_corrupt_partial_and_oversized_stores() {
    let _lifecycle = LIFECYCLE_LOCK.lock().await;
    for (case, bytes) in [
        ("corrupt", b"not-json".to_vec()),
        (
            "oversized",
            vec![b'x'; crate::open_subsonic::profile::MAX_PROFILE_BYTES as usize + 1],
        ),
    ] {
        let id = NEXT_ROOT.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "yututui-open-subsonic-reset-{case}-{}-{id}",
            std::process::id()
        ));
        let paths = OpenSubsonicPaths::for_data_root(root.clone());
        crate::util::safe_fs::write_owner_only_atomic(paths.profile(), &bytes).unwrap();

        assert_eq!(
            read_status(&paths).unwrap().kind,
            OpenSubsonicStatusKind::NeedsAttention
        );
        assert_eq!(
            remove_profile(&paths).unwrap().kind,
            OpenSubsonicStatusKind::Off
        );
        assert_eq!(
            read_status(&paths).unwrap().kind,
            OpenSubsonicStatusKind::Off
        );
        let _ = std::fs::remove_dir_all(root);
    }
}

#[cfg(unix)]
#[tokio::test]
async fn confirmed_remove_rejects_a_symlink_without_touching_its_target() {
    use std::os::unix::fs::symlink;

    let _lifecycle = LIFECYCLE_LOCK.lock().await;
    let id = NEXT_ROOT.fetch_add(1, Ordering::Relaxed);
    let root = std::env::temp_dir().join(format!(
        "yututui-open-subsonic-reset-link-{}-{id}",
        std::process::id()
    ));
    let paths = OpenSubsonicPaths::for_data_root(root.clone());
    crate::util::safe_fs::ensure_private_dir(paths.root()).unwrap();
    let external = root.join("outside-secret");
    std::fs::write(&external, b"must-stay").unwrap();
    symlink(&external, paths.profile()).unwrap();

    assert!(remove_profile(&paths).is_err());
    assert_eq!(std::fs::read(&external).unwrap(), b"must-stay");
    assert!(paths.profile().is_symlink());
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn setup_input_is_move_only_and_accepts_secret_credential() {
    let input = SetupInput::new(
        "Server",
        "https://music.example.test/",
        false,
        None,
        ServerCredential::api_key(SecretString::from("secret".to_owned())).unwrap(),
        SetupIdentityIntent::Create,
    );
    assert_eq!(input.identity_intent, SetupIdentityIntent::Create);
}

#[test]
fn same_account_update_requires_matching_exact_owner_for_password_candidates() {
    let previous_password =
        ServerCredential::password("alice", SecretString::from("old-password".to_owned())).unwrap();
    let same_password =
        ServerCredential::password("alice", SecretString::from("new-password".to_owned())).unwrap();
    let changed_password =
        ServerCredential::password("bob", SecretString::from("new-password".to_owned())).unwrap();
    assert_eq!(
        require_same_account_owner(&previous_password, &same_password),
        Ok(())
    );
    assert_eq!(
        require_same_account_owner(&previous_password, &changed_password),
        Err(ServiceError::InvalidSetup)
    );

    let mut bound_api_key =
        ServerCredential::api_key(SecretString::from("old-api-key".to_owned())).unwrap();
    bound_api_key.bind_api_key_username("alice").unwrap();
    assert_eq!(
        require_same_account_owner(&bound_api_key, &same_password),
        Ok(()),
        "an API-key to password update may preserve scope only with the same exact owner"
    );
    let unbound_api_key =
        ServerCredential::api_key(SecretString::from("legacy-api-key".to_owned())).unwrap();
    assert_eq!(
        require_same_account_owner(&unbound_api_key, &same_password),
        Err(ServiceError::InvalidSetup),
        "legacy API keys without owner evidence must use Replace"
    );
}

#[tokio::test]
async fn api_key_setup_binds_token_info_owner_without_sending_legacy_username_auth() {
    let _lifecycle = LIFECYCLE_LOCK.lock().await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = tokio::spawn(async move {
        let replies = [
            (
                "/rest/getOpenSubsonicExtensions.view?",
                r#"{"subsonic-response":{"status":"ok","openSubsonicExtensions":[{"name":"apiKeyAuthentication","versions":[1]}]}}"#,
            ),
            (
                "/rest/ping.view?",
                r#"{"subsonic-response":{"status":"ok","version":"1.16.1"}}"#,
            ),
            (
                "/rest/tokenInfo.view?",
                r#"{"subsonic-response":{"status":"ok","tokenInfo":{"username":"alice"}}}"#,
            ),
        ];
        for (expected_endpoint, body) in replies {
            let (mut stream, _) = listener.accept().await.unwrap();
            let request = read_request_head(&mut stream).await;
            let target = request
                .lines()
                .next()
                .and_then(|line| line.split_ascii_whitespace().nth(1))
                .unwrap();
            assert!(target.starts_with(expected_endpoint), "{target}");
            if expected_endpoint.contains("tokenInfo") {
                let query = reqwest::Url::parse(&format!("http://fixture{target}")).unwrap();
                let fields = query.query_pairs().collect::<Vec<_>>();
                assert!(
                    fields
                        .iter()
                        .any(|(name, value)| name == "apiKey" && value == "setup-api-key")
                );
                assert!(fields.iter().all(|(name, _)| name != "u"));
            }
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(response.as_bytes()).await.unwrap();
        }
    });
    let id = NEXT_ROOT.fetch_add(1, Ordering::Relaxed);
    let root = std::env::temp_dir().join(format!(
        "yututui-open-subsonic-token-info-{}-{id}",
        std::process::id()
    ));
    crate::util::safe_fs::ensure_private_dir(&root).unwrap();
    let paths = OpenSubsonicPaths::for_data_root(root.clone());
    let input = SetupInput::new(
        "API-key server",
        format!("http://127.0.0.1:{port}/"),
        true,
        None,
        ServerCredential::api_key(SecretString::from("setup-api-key".to_owned())).unwrap(),
        SetupIdentityIntent::Create,
    );

    let prepared = test_and_prepare_setup(&paths, input).await.unwrap();
    commit_setup(&paths, prepared).unwrap();
    server.await.unwrap();
    let stored = load_store_set(&paths).unwrap().unwrap();
    assert_eq!(
        stored
            .private_state
            .credential()
            .username()
            .unwrap()
            .expose_secret(),
        "alice"
    );
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn unbound_api_key_update_cannot_preserve_account_scope_without_token_info() {
    let _lifecycle = LIFECYCLE_LOCK.lock().await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = tokio::spawn(async move {
        let replies = [
            (
                "/rest/getOpenSubsonicExtensions.view?",
                r#"{"subsonic-response":{"status":"ok","openSubsonicExtensions":[]}}"#,
            ),
            (
                "/rest/ping.view?",
                r#"{"subsonic-response":{"status":"ok","version":"1.16.1"}}"#,
            ),
            (
                "/rest/getOpenSubsonicExtensions.view?",
                r#"{"subsonic-response":{"status":"ok","openSubsonicExtensions":[]}}"#,
            ),
            (
                "/rest/ping.view?",
                r#"{"subsonic-response":{"status":"ok","version":"1.16.1"}}"#,
            ),
        ];
        for (expected_endpoint, body) in replies {
            let (mut stream, _) = listener.accept().await.unwrap();
            let request = read_request_head(&mut stream).await;
            let target = request
                .lines()
                .next()
                .and_then(|line| line.split_ascii_whitespace().nth(1))
                .unwrap();
            assert!(target.starts_with(expected_endpoint), "{target}");
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(response.as_bytes()).await.unwrap();
        }
    });
    let id = NEXT_ROOT.fetch_add(1, Ordering::Relaxed);
    let root = std::env::temp_dir().join(format!(
        "yututui-open-subsonic-unbound-api-key-{}-{id}",
        std::process::id()
    ));
    crate::util::safe_fs::ensure_private_dir(&root).unwrap();
    let paths = OpenSubsonicPaths::for_data_root(root.clone());
    let create = SetupInput::new(
        "Legacy API-key server",
        format!("http://127.0.0.1:{port}/"),
        true,
        None,
        ServerCredential::api_key(SecretString::from("old-api-key".to_owned())).unwrap(),
        SetupIdentityIntent::Create,
    );
    let initial = commit_setup(
        &paths,
        test_and_prepare_setup(&paths, create).await.unwrap(),
    )
    .unwrap();
    let update = SetupInput::new(
        "Updated API-key server",
        format!("http://127.0.0.1:{port}/"),
        true,
        None,
        ServerCredential::api_key(SecretString::from("new-api-key".to_owned())).unwrap(),
        SetupIdentityIntent::UpdateSameServerAndAccount,
    );

    assert!(matches!(
        test_and_prepare_setup(&paths, update).await,
        Err(ServiceError::InvalidSetup)
    ));
    server.await.unwrap();
    let retained = read_status(&paths).unwrap();
    assert_eq!(retained.backend_id, initial.backend_id);
    assert_eq!(retained.account_scope_id, initial.account_scope_id);
    assert_eq!(retained.display_name, initial.display_name);
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn same_account_setup_requeues_playlist_access_for_verification_once() {
    let _lifecycle = LIFECYCLE_LOCK.lock().await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = tokio::spawn(async move {
        let replies = [
            r#"{"subsonic-response":{"status":"ok","openSubsonicExtensions":[]}}"#,
            r#"{"subsonic-response":{"status":"ok","version":"1.16.1"}}"#,
            r#"{"subsonic-response":{"status":"ok","openSubsonicExtensions":[]}}"#,
            r#"{"subsonic-response":{"status":"ok","version":"1.16.1"}}"#,
        ];
        for body in replies {
            let (mut stream, _) = listener.accept().await.unwrap();
            let _request = read_request_head(&mut stream).await;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(response.as_bytes()).await.unwrap();
        }
    });
    let id = NEXT_ROOT.fetch_add(1, Ordering::Relaxed);
    let root = std::env::temp_dir().join(format!(
        "yututui-open-subsonic-playlist-requeue-{}-{id}",
        std::process::id()
    ));
    crate::util::safe_fs::ensure_private_dir(&root).unwrap();
    let paths = OpenSubsonicPaths::for_data_root(root.clone());
    let create = SetupInput::new(
        "Test server",
        format!("http://127.0.0.1:{port}/"),
        true,
        None,
        ServerCredential::password("alice", SecretString::from("old-secret".to_owned())).unwrap(),
        SetupIdentityIntent::Create,
    );
    commit_setup(
        &paths,
        test_and_prepare_setup(&paths, create).await.unwrap(),
    )
    .unwrap();

    let mut current = load_store_set(&paths).unwrap().unwrap();
    let expected = current.revisions();
    let local_playlist_id = crate::personal_state::PlaylistId::new("local").unwrap();
    current
        .bridge_state
        .upsert_playlist_link(PlaylistLink {
            local_playlist_id: local_playlist_id.clone(),
            server_playlist_id: ServerPlaylistId::new("playlist").unwrap(),
            managed_by_yututui: true,
            state: PlaylistLinkState::AccessNeedsAttention,
            content_needs_attention: false,
            shadow: PlaylistShadow {
                name: "Playlist".to_owned(),
                occurrences: Vec::new(),
                verified_at_unix: 100,
            },
        })
        .unwrap();
    current
        .bridge_state
        .queue_playlist_projection(
            local_playlist_id.clone(),
            PendingPlaylistProjection {
                desired_name: "Local edit".to_owned(),
                ordered_entry_ids: Vec::new(),
                ordered_item_ids: Vec::new(),
                stage: PendingPlaylistProjectionStage::NeedsAttention,
                base_remote_fingerprint: "fingerprint".to_owned(),
            },
        )
        .unwrap();
    assert_eq!(
        current
            .bridge_state
            .playlist_projections_needing_attention(),
        1,
        "link and projection attention for one local playlist are counted once"
    );
    commit_store_set(&paths, expected, &mut current).unwrap();

    let update = SetupInput::new(
        "Updated server",
        format!("http://127.0.0.1:{port}/"),
        true,
        None,
        ServerCredential::password("alice", SecretString::from("new-secret".to_owned())).unwrap(),
        SetupIdentityIntent::UpdateSameServerAndAccount,
    );
    let prepared = test_and_prepare_setup(&paths, update).await.unwrap();
    let candidate = prepared.store_set.as_ref().unwrap();
    assert_eq!(
        candidate
            .bridge_state
            .playlist_link(&local_playlist_id)
            .map(|link| link.state),
        Some(PlaylistLinkState::Linked)
    );
    assert_eq!(
        candidate
            .bridge_state
            .pending_playlist_projections()
            .get(&local_playlist_id)
            .map(|pending| pending.stage),
        Some(PendingPlaylistProjectionStage::Queued)
    );
    assert_eq!(
        candidate
            .bridge_state
            .playlist_projections_needing_attention(),
        0
    );
    commit_setup(&paths, prepared).unwrap();
    server.await.unwrap();

    let durable = load_store_set(&paths).unwrap().unwrap();
    assert_eq!(
        durable
            .bridge_state
            .playlist_link(&local_playlist_id)
            .map(|link| link.state),
        Some(PlaylistLinkState::Linked)
    );
    assert_eq!(
        durable
            .bridge_state
            .pending_playlist_projections()
            .get(&local_playlist_id)
            .map(|pending| pending.stage),
        Some(PendingPlaylistProjectionStage::Queued)
    );
    let _ = std::fs::remove_dir_all(root);
}

struct LifecycleFixture {
    root: std::path::PathBuf,
    paths: OpenSubsonicPaths,
    server: tokio::task::JoinHandle<()>,
    active: Option<OpenSubsonicRuntime>,
    backend_id: BackendId,
    account_scope_id: AccountScopeId,
}

impl Drop for LifecycleFixture {
    fn drop(&mut self) {
        self.active.take();
        self.server.abort();
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

async fn lifecycle_fixture(label: &str) -> LifecycleFixture {
    clear_current_runtime();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                break;
            };
            let request = read_request_head(&mut stream).await;
            let (content_type, body): (&str, &[u8]) =
                if request.contains("/rest/getOpenSubsonicExtensions") {
                    (
                        "application/json",
                        br#"{"subsonic-response":{"status":"ok","openSubsonicExtensions":[]}}"#,
                    )
                } else if request.contains("/rest/stream") {
                    ("audio/mpeg", b"fake-audio")
                } else {
                    (
                        "application/json",
                        br#"{"subsonic-response":{"status":"ok","version":"1.16.1"}}"#,
                    )
                };
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            if stream.write_all(response.as_bytes()).await.is_ok() {
                let _ = stream.write_all(body).await;
            }
        }
    });

    let id = NEXT_ROOT.fetch_add(1, Ordering::Relaxed);
    let root = std::env::temp_dir().join(format!(
        "yututui-open-subsonic-lifecycle-{label}-{}-{id}",
        std::process::id()
    ));
    let paths = OpenSubsonicPaths::for_data_root(root.clone());
    let backend_id = BackendId::new(format!("lifecycle-backend-{id}")).unwrap();
    let account_scope_id = AccountScopeId::new(format!("lifecycle-account-{id}")).unwrap();
    let profile = OpenSubsonicProfile::with_ids(
        0,
        backend_id.clone(),
        account_scope_id.clone(),
        "Lifecycle server",
        ConfiguredPrivateOrigin::new(&format!("http://127.0.0.1:{port}/"), true).unwrap(),
        None,
    )
    .unwrap();
    let private_state = OpenSubsonicPrivateState::new(
        backend_id.clone(),
        account_scope_id.clone(),
        ServerCredential::api_key(SecretString::from("lifecycle-secret".to_owned())).unwrap(),
    );
    let bridge_state = OpenSubsonicBridgeState::new(backend_id.clone(), account_scope_id.clone());
    let mut store_set = OpenSubsonicStoreSet::new(profile, private_state, bridge_state).unwrap();
    commit_store_set(&paths, StoreRevisions::MISSING, &mut store_set).unwrap();
    let active = load_actor(&paths).await.unwrap().unwrap();
    active.activate();
    LifecycleFixture {
        root,
        paths,
        server,
        active: Some(active),
        backend_id,
        account_scope_id,
    }
}

async fn read_request_head(stream: &mut tokio::net::TcpStream) -> String {
    let mut request = Vec::new();
    let mut byte = [0_u8; 1];
    while request.len() < 32 * 1024 {
        if stream.read(&mut byte).await.unwrap_or(0) == 0 {
            break;
        }
        request.push(byte[0]);
        if request.ends_with(b"\r\n\r\n") {
            break;
        }
    }
    String::from_utf8(request).unwrap()
}

fn lifecycle_target(
    fixture: &LifecycleFixture,
    item_id: &str,
) -> crate::playback_target::CredentialedPlaybackRef {
    crate::playback_target::CredentialedPlaybackRef::OpenSubsonic {
        backend_id: fixture.backend_id.as_str().to_owned(),
        account_scope_id: fixture.account_scope_id.as_str().to_owned(),
        item_id: item_id.to_owned(),
    }
}

fn lifecycle_track(
    fixture: &LifecycleFixture,
    item_id: &str,
    started_unix: i64,
) -> crate::scrobble::ScrobbleTrack {
    crate::scrobble::ScrobbleTrack {
        key: item_id.to_owned(),
        open_subsonic_item: Some(OpenSubsonicItemRef::new(
            fixture.backend_id.clone(),
            fixture.account_scope_id.clone(),
            crate::open_subsonic::ItemId::new(item_id).unwrap(),
        )),
        artist: "Lifecycle artist".to_owned(),
        title: "Lifecycle song".to_owned(),
        album: None,
        duration_secs: Some(180),
        origin_url: None,
        started_unix,
    }
}

#[tokio::test]
async fn dormant_and_discarded_candidates_leave_the_active_runtime_usable() {
    let _lifecycle = LIFECYCLE_LOCK.lock().await;
    let fixture = lifecycle_fixture("discard").await;
    let active = fixture.active.as_ref().unwrap();
    let active_handle = active.handle();
    let active_provider = active.route_provider();
    let route = active_provider
        .open_route(lifecycle_target(&fixture, "before-candidate"), 1)
        .await
        .unwrap();
    let (route_url, _route_lease) = route.into_parts();
    let route_url = route_url.into_string();

    let candidate = load_actor(&fixture.paths).await.unwrap().unwrap();
    assert_eq!(
        current_handle()
            .unwrap()
            .profile_summary()
            .await
            .unwrap()
            .display_name,
        "Lifecycle server"
    );
    assert_eq!(
        active_handle.profile_summary().await.unwrap().display_name,
        "Lifecycle server"
    );
    assert_eq!(
        reqwest::get(&route_url).await.unwrap().status(),
        reqwest::StatusCode::OK
    );
    let route_after_load = active_provider
        .open_route(lifecycle_target(&fixture, "after-candidate"), 2)
        .await
        .unwrap();
    let (route_after_load_url, _route_after_load_lease) = route_after_load.into_parts();
    let route_after_load_url = route_after_load_url.into_string();

    drop(candidate);
    assert!(active_handle.profile_summary().await.is_ok());
    assert_eq!(
        reqwest::get(&route_after_load_url).await.unwrap().status(),
        reqwest::StatusCode::OK
    );
    assert!(
        active_provider
            .open_route(lifecycle_target(&fixture, "after-discard"), 3)
            .await
            .is_ok()
    );
}

#[tokio::test]
async fn activation_rebases_then_replaces_the_active_runtime_once() {
    let _lifecycle = LIFECYCLE_LOCK.lock().await;
    let fixture = lifecycle_fixture("activate").await;
    let active = fixture.active.as_ref().unwrap();
    let old_handle = active.handle();
    let old_provider = active.route_provider();
    let usable_route = old_provider
        .open_route(lifecycle_target(&fixture, "usable-before-activation"), 10)
        .await
        .unwrap();
    let (usable_url, _usable_lease) = usable_route.into_parts();
    let usable_url = usable_url.into_string();
    assert_eq!(
        reqwest::get(&usable_url).await.unwrap().status(),
        reqwest::StatusCode::OK
    );
    let revoked_route = old_provider
        .open_route(lifecycle_target(&fixture, "revoked-on-activation"), 11)
        .await
        .unwrap();
    let (revoked_url, _revoked_lease) = revoked_route.into_parts();
    let revoked_url = revoked_url.into_string();

    let candidate = load_actor(&fixture.paths).await.unwrap().unwrap();
    let candidate_handle = candidate.handle();
    let first_receipt = old_handle
        .queue_scrobble(
            "old-owner-advanced-store".to_owned(),
            OpenSubsonicScrobbleKind::Submission,
            lifecycle_track(&fixture, "history-song", 100),
        )
        .unwrap();
    first_receipt.await.unwrap().unwrap();

    candidate.activate();
    candidate.activate();
    let second_receipt = candidate_handle
        .queue_scrobble(
            "candidate-after-rebase".to_owned(),
            OpenSubsonicScrobbleKind::Submission,
            lifecycle_track(&fixture, "history-song", 101),
        )
        .unwrap();
    second_receipt.await.unwrap().unwrap();

    assert_eq!(
        old_handle.profile_summary().await.unwrap_err(),
        ServerError::Offline
    );
    assert_eq!(
        old_provider
            .open_route(lifecycle_target(&fixture, "old-provider"), 12)
            .await
            .unwrap_err()
            .reason(),
        "route_provider_unavailable"
    );
    assert_eq!(
        reqwest::get(&revoked_url).await.unwrap().status(),
        reqwest::StatusCode::NOT_FOUND
    );

    let durable = load_store_set(&fixture.paths).unwrap().unwrap();
    assert_eq!(
        durable.bridge_state.outbound_scrobbles().len(),
        2,
        "activation rebase must preserve the old actor's durable bridge mutation"
    );
    assert!(candidate_handle.profile_summary().await.is_ok());
    let candidate_route = candidate
        .route_provider()
        .open_route(lifecycle_target(&fixture, "candidate-route"), 13)
        .await
        .unwrap();
    let (candidate_url, _candidate_lease) = candidate_route.into_parts();
    let candidate_url = candidate_url.into_string();
    assert_eq!(
        reqwest::get(&candidate_url).await.unwrap().status(),
        reqwest::StatusCode::OK,
        "a repeated activation must not revoke the already-current candidate"
    );
    drop(candidate);
}

#[tokio::test]
async fn tested_setup_commits_all_stores_and_remove_is_local_only() {
    let _lifecycle = LIFECYCLE_LOCK.lock().await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = tokio::spawn(async move {
        let replies = [
            r#"{"subsonic-response":{"status":"ok","openSubsonicExtensions":[]}}"#,
            r#"{"subsonic-response":{"status":"ok","version":"1.16.1"}}"#,
        ];
        for body in replies.into_iter().cycle().take(12) {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut byte = [0_u8; 1];
            while request.len() < 16 * 1024 {
                if stream.read(&mut byte).await.unwrap() == 0 {
                    break;
                }
                request.push(byte[0]);
                if request.ends_with(b"\r\n\r\n") {
                    break;
                }
            }
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{body}"
            );
            stream.write_all(response.as_bytes()).await.unwrap();
        }
    });
    let id = NEXT_ROOT.fetch_add(1, Ordering::Relaxed);
    let root = std::env::temp_dir().join(format!(
        "yututui-open-subsonic-service-{}-{id}",
        std::process::id()
    ));
    crate::util::safe_fs::ensure_private_dir(&root).unwrap();
    let paths = OpenSubsonicPaths::for_data_root(root.clone());
    let input = SetupInput::new(
        "Test server",
        format!("http://127.0.0.1:{port}/"),
        true,
        None,
        ServerCredential::password("alice", SecretString::from("secret".to_owned())).unwrap(),
        SetupIdentityIntent::Create,
    );
    let prepared = test_and_prepare_setup(&paths, input).await.unwrap();
    let status = commit_setup(&paths, prepared).unwrap();
    assert_eq!(status.kind, OpenSubsonicStatusKind::UpToDate);
    let original_backend = status.backend_id.clone().unwrap();
    let original_account = status.account_scope_id.clone().unwrap();
    assert_eq!(read_status(&paths).unwrap().kind, status.kind);
    assert_eq!(
        test_connection(&paths).await.unwrap().kind,
        OpenSubsonicStatusKind::UpToDate
    );
    let retained_item = crate::open_subsonic::ItemId::new("retained-rating").unwrap();
    let mut store_with_observation = load_store_set(&paths).unwrap().unwrap();
    let expected = store_with_observation.revisions();
    store_with_observation
        .bridge_state
        .upsert_rating_shadow(
            retained_item.clone(),
            RatingShadow {
                raw: RawServerRating {
                    user_rating: Some(3),
                    starred: true,
                },
                observed_at_unix: 10,
                confirmed_operation_id: None,
            },
        )
        .unwrap();
    commit_store_set(&paths, expected, &mut store_with_observation).unwrap();
    let old_runtime = load_actor(&paths).await.unwrap().unwrap();
    let old_handle = old_runtime.handle();
    let old_provider = old_runtime.route_provider();
    old_runtime.activate();
    let old_target = crate::playback_target::CredentialedPlaybackRef::OpenSubsonic {
        backend_id: original_backend.as_str().to_owned(),
        account_scope_id: original_account.as_str().to_owned(),
        item_id: "old-route-item".to_owned(),
    };
    let old_route = old_provider
        .open_route(old_target.clone(), 1)
        .await
        .unwrap();
    let (old_route_url, _old_route_lease) = old_route.into_parts();
    let old_route_url = old_route_url.into_string();

    let update = SetupInput::new(
        "Renamed server",
        format!("http://127.0.0.1:{port}/"),
        true,
        None,
        ServerCredential::password("alice", SecretString::from("updated-secret".to_owned()))
            .unwrap(),
        SetupIdentityIntent::UpdateSameServerAndAccount,
    );
    crate::open_subsonic::transaction::fail_after_commit_marker_once_for_test();
    let prepared_update = test_and_prepare_setup(&paths, update).await.unwrap();
    assert_eq!(
        prepared_update
            .store_set
            .as_ref()
            .unwrap()
            .bridge_state
            .rating_shadow(&retained_item)
            .unwrap()
            .raw,
        RawServerRating {
            user_rating: Some(3),
            starred: true,
        }
    );
    assert_eq!(
        commit_setup(&paths, prepared_update),
        Err(ServiceError::Store(StoreError::StorageUnavailable))
    );
    assert_eq!(
        old_handle.profile_summary().await.unwrap_err(),
        ServerError::Offline
    );
    assert_eq!(
        old_provider
            .open_route(old_target, 2)
            .await
            .unwrap_err()
            .reason(),
        "route_provider_unavailable"
    );
    assert_eq!(
        reqwest::get(old_route_url).await.unwrap().status(),
        reqwest::StatusCode::NOT_FOUND
    );

    // The next owner load rolls the committed candidate forward, but never revives the old
    // handle or route.
    let recovered_store = load_store_set(&paths).unwrap().unwrap();
    assert!(
        recovered_store
            .bridge_state
            .rating_shadow(&retained_item)
            .is_some()
    );
    let updated = read_status(&paths).unwrap();
    assert_eq!(updated.kind, OpenSubsonicStatusKind::UpToDate);
    assert_eq!(updated.display_name.as_deref(), Some("Renamed server"));
    assert_eq!(updated.backend_id.as_ref(), Some(&original_backend));
    assert_eq!(updated.account_scope_id.as_ref(), Some(&original_account));
    assert_eq!(
        old_handle.profile_summary().await.unwrap_err(),
        ServerError::Offline
    );
    drop(old_runtime);

    let replacement = SetupInput::new(
        "Replacement server",
        format!("http://127.0.0.1:{port}/"),
        true,
        None,
        ServerCredential::password("bob", SecretString::from("replacement-secret".to_owned()))
            .unwrap(),
        SetupIdentityIntent::ReplaceServerOrAccount,
    );
    let replaced = commit_setup(
        &paths,
        test_and_prepare_setup(&paths, replacement).await.unwrap(),
    )
    .unwrap();
    assert_ne!(replaced.backend_id.as_ref(), Some(&original_backend));
    assert_ne!(replaced.account_scope_id.as_ref(), Some(&original_account));
    assert!(
        load_store_set(&paths)
            .unwrap()
            .unwrap()
            .bridge_state
            .rating_shadow(&retained_item)
            .is_none()
    );
    let removal_runtime = load_actor(&paths).await.unwrap().unwrap();
    let removal_handle = removal_runtime.handle();
    removal_runtime.activate();
    assert_eq!(
        remove_profile(&paths).unwrap().kind,
        OpenSubsonicStatusKind::Off
    );
    assert_eq!(
        removal_handle.profile_summary().await.unwrap_err(),
        ServerError::Offline
    );
    drop(removal_runtime);
    assert_eq!(
        read_status(&paths).unwrap().kind,
        OpenSubsonicStatusKind::Off
    );
    server.await.unwrap();
    let _ = std::fs::remove_dir_all(root);
}
