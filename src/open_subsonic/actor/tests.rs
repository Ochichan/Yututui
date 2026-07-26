use std::sync::atomic::{AtomicU64, Ordering};

use age::secrecy::SecretString;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

use super::*;
use crate::open_subsonic::bridge_store::RatingShadow;
use crate::open_subsonic::rating::RawServerRating;

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
    };
    let rendered = format!("{status:?}");
    assert!(!rendered.contains("https://"));
    assert!(!rendered.contains("sentinel-secret"));
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
        ServerCredential::api_key(SecretString::from("secret".to_owned())).unwrap(),
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
        ServerCredential::api_key(SecretString::from("updated-secret".to_owned())).unwrap(),
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
        ServerCredential::api_key(SecretString::from("replacement-secret".to_owned())).unwrap(),
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
