use super::*;

#[test]
fn active_cache_policy_write_failure_emits_forced_ram_terminal_not_transport_closed() {
    let mut state = DispatchState {
        issued_file_generation: 7,
        active_file_generation: Some(7),
        last_confirmed_time: 3_600.25,
        paused: true,
        ..DispatchState::default()
    };
    state.cache = None;

    let events = terminal_events(cache_io_failure_exit(
        &state,
        super::super::super::long_form_seek::CacheReason::PropertyVerificationFailed,
    ));

    assert!(matches!(
        events.as_slice(),
        [PlayerEvent::CacheEmergency {
            file_generation: 7,
            position_secs,
            paused: true,
            reason: super::super::super::long_form_seek::CacheReason::PropertyVerificationFailed,
        }] if (*position_secs - 3_600.25).abs() < f64::EPSILON
    ));
    assert_eq!(events[0].file_generation(), None);
}

#[test]
fn cache_policy_write_failure_always_carries_origin_in_a_process_scoped_terminal() {
    let state = DispatchState {
        issued_file_generation: 8,
        active_file_generation: Some(7),
        last_confirmed_time: 3_600.25,
        paused: true,
        ..DispatchState::default()
    };

    let events = terminal_events(cache_io_failure_exit(
        &state,
        super::super::super::long_form_seek::CacheReason::PropertyVerificationFailed,
    ));

    assert!(matches!(
        events.as_slice(),
        [PlayerEvent::CacheEmergency {
            file_generation: 7,
            position_secs,
            paused: true,
            reason: super::super::super::long_form_seek::CacheReason::PropertyVerificationFailed,
        }] if (*position_secs - 3_600.25).abs() < f64::EPSILON
    ));
    assert_eq!(events[0].file_generation(), None);
}

#[test]
fn replacement_reset_rejection_retains_the_old_origin_for_owner_reconciliation() {
    let events = terminal_events(ActorExit::CacheEmergency {
        file_generation: 7,
        position_secs: 3_600.25,
        paused: true,
        reason: super::super::super::long_form_seek::CacheReason::DisableFailed,
    });

    assert!(matches!(
        events.as_slice(),
        [PlayerEvent::CacheEmergency {
            file_generation: 7,
            reason: super::super::super::long_form_seek::CacheReason::DisableFailed,
            ..
        }]
    ));
}
