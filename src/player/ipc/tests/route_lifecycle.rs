use super::*;

#[derive(Clone)]
struct TestRevocation(std::sync::Arc<std::sync::atomic::AtomicBool>);

impl crate::playback_target::PlaybackRouteRevocation for TestRevocation {
    fn revoke(&self) {
        self.0.store(true, std::sync::atomic::Ordering::Release);
    }
}

fn test_route_lease() -> (
    crate::playback_target::PlaybackRouteLease,
    std::sync::Arc<std::sync::atomic::AtomicBool>,
) {
    let revoked = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let revocation: std::sync::Arc<dyn crate::playback_target::PlaybackRouteRevocation> =
        std::sync::Arc::new(TestRevocation(std::sync::Arc::clone(&revoked)));
    (
        crate::playback_target::PlaybackRouteLease::new(revocation),
        revoked,
    )
}

struct RecordingRouteProvider(std::sync::Arc<std::sync::atomic::AtomicU64>);

impl crate::playback_target::PlaybackRouteProvider for RecordingRouteProvider {
    fn open_route(
        &self,
        _target: crate::playback_target::CredentialedPlaybackRef,
        file_generation: u64,
    ) -> crate::playback_target::PlaybackRouteFuture {
        self.0
            .store(file_generation, std::sync::atomic::Ordering::Release);
        Box::pin(async {
            crate::playback_target::RoutedPlayback::new(32123, "a".repeat(32), test_route_lease().0)
        })
    }
}

#[tokio::test]
async fn credentialed_provider_receives_the_exact_reserved_generation() {
    let generation = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let provider = crate::playback_target::PlaybackRouteProviderHandle::new(std::sync::Arc::new(
        RecordingRouteProvider(std::sync::Arc::clone(&generation)),
    ));
    let (_generation_tx, generation_rx) = watch::channel(9);
    let outcome = validate_load_until_superseded(
        crate::playback_target::PlaybackDestination::Credentialed(
            crate::playback_target::CredentialedPlaybackRef::OpenSubsonic {
                backend_id: "backend".to_owned(),
                account_scope_id: "account".to_owned(),
                item_id: "item".to_owned(),
            },
        ),
        9,
        generation_rx,
        provider,
    )
    .await;

    assert_eq!(generation.load(std::sync::atomic::Ordering::Acquire), 9);
    assert!(matches!(
        outcome,
        LoadValidationOutcome::Validated {
            route_lease: Some(_),
            ..
        }
    ));
}

#[tokio::test]
async fn direct_loopback_target_cannot_forge_a_provider_route() {
    let (_generation_tx, generation_rx) = watch::channel(1);
    let outcome = validate_load_until_superseded(
        crate::playback_target::PlaybackDestination::Direct(
            "http://127.0.0.1:7890/stream/forged".to_owned(),
        ),
        1,
        generation_rx,
        crate::playback_target::PlaybackRouteProviderHandle::disabled(),
    )
    .await;

    assert!(matches!(
        outcome,
        LoadValidationOutcome::Rejected(reason) if reason == "blocked_destination"
    ));
}

#[test]
fn replacing_load_revokes_the_active_route_before_validation() {
    let (lease, revoked) = test_route_lease();
    let mut state = DispatchState {
        issued_file_generation: 7,
        active_file_generation: Some(7),
        ..DispatchState::default()
    };
    state.playback_route_leases.insert(7, lease);
    let mut validation = None;
    let mut backlog = VecDeque::new();
    let mut flight = None;

    accept_actor_command(
        &mut state,
        PlayerCmd::load(
            "https://example.invalid/replacement",
            super::super::super::MediaSourceContext::OnDemand,
        ),
        &mut validation,
        &mut backlog,
        &mut flight,
    );

    assert!(revoked.load(std::sync::atomic::Ordering::Acquire));
    assert!(state.playback_route_leases.is_empty());
}

#[test]
fn stale_end_file_cannot_revoke_the_newer_route() {
    let emit: EventSink = std::sync::Arc::new(|_| {});
    let (lease, revoked) = test_route_lease();
    let mut state = DispatchState {
        issued_file_generation: 2,
        active_file_generation: Some(2),
        active_playlist_entry_id: Some(22),
        ..DispatchState::default()
    };
    state.entry_generations.insert(11, 1);
    state.entry_generations.insert(22, 2);
    state.playback_route_leases.insert(2, lease);

    dispatch_incoming(
        r#"{"event":"end-file","reason":"stop","playlist_entry_id":11}"#,
        &emit,
        &mut state,
    );

    assert!(!revoked.load(std::sync::atomic::Ordering::Acquire));
    assert!(state.playback_route_leases.contains_key(&2));
}

#[test]
fn correlated_end_file_revokes_its_route() {
    let emit: EventSink = std::sync::Arc::new(|_| {});
    let (lease, revoked) = test_route_lease();
    let mut state = DispatchState {
        issued_file_generation: 2,
        active_file_generation: Some(2),
        active_playlist_entry_id: Some(22),
        ..DispatchState::default()
    };
    state.entry_generations.insert(22, 2);
    state.playback_route_leases.insert(2, lease);

    dispatch_incoming(
        r#"{"event":"end-file","reason":"eof","playlist_entry_id":22}"#,
        &emit,
        &mut state,
    );

    assert!(revoked.load(std::sync::atomic::Ordering::Acquire));
    assert!(state.playback_route_leases.is_empty());
}
