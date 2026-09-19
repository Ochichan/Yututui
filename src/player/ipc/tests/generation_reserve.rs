use super::*;

fn reserve_next(state: &mut DispatchState) -> u64 {
    reserve_published_file_generation(state, 0)
}

fn never_validated() -> LoadValidationOutcome {
    LoadValidationOutcome::Validated {
        url: "never".to_owned(),
        route_lease: None,
    }
}

#[test]
fn actor_fifo_reservations_match_two_queued_load_admissions() {
    let mut state = DispatchState::default();
    let load_b = reserve_next(&mut state);
    let load_c = reserve_next(&mut state);
    assert_eq!((load_b, load_c), (1, 2));
    assert_eq!(state.admitted_file_generation, 2);
    assert_eq!(state.issued_file_generation, 0);
}

#[test]
fn actor_fifo_reservations_match_stop_then_load_batch() {
    let mut state = DispatchState::default();
    let stop = reserve_next(&mut state);
    state.issued_file_generation = stop;
    let load = reserve_next(&mut state);
    assert_eq!((stop, load), (1, 2));
    assert_eq!(state.issued_file_generation, 1);
    assert_eq!(state.admitted_file_generation, 2);
}

#[test]
fn extra_deck_inherits_owner_generation_then_wrapping_adds() {
    let mut state = DispatchState::default();
    inherit_owner_file_generation(&mut state, 5);
    assert_eq!(state.admitted_file_generation, 5);
    assert_eq!(state.issued_file_generation, 5);
    assert_eq!(reserve_next(&mut state), 6);
}

#[test]
fn extra_load_reuses_owner_generation_when_already_inherited() {
    let mut state = DispatchState::default();
    inherit_owner_file_generation(&mut state, 2);
    assert_eq!(reserve_published_file_generation(&mut state, 2), 2);
    assert_eq!(state.admitted_file_generation, 2);
    assert_eq!(state.issued_file_generation, 2);
}

#[test]
fn extra_load_snaps_forward_to_owner_published_generation() {
    let mut state = DispatchState {
        issued_file_generation: 1,
        admitted_file_generation: 1,
        ..DispatchState::default()
    };
    assert_eq!(reserve_published_file_generation(&mut state, 2), 2);
    assert_eq!(state.admitted_file_generation, 2);
    assert_eq!(state.issued_file_generation, 2);
}

#[tokio::test]
async fn owner_published_recovery_reserve_keeps_current_seek_dispatchable() {
    let mut state = DispatchState {
        issued_file_generation: 7,
        admitted_file_generation: 7,
        active_file_generation: Some(7),
        file_loaded_generation: Some(7),
        playback_ready_generation: Some(7),
        ..DispatchState::default()
    };
    let candidate = reserve_published_file_generation(&mut state, 8);
    assert_eq!(candidate, 8);
    assert_eq!(
        state.issued_file_generation, 7,
        "pending validation must not mark issued as a generation never sent to mpv"
    );
    let task = tokio::spawn(async {
        std::future::pending::<()>().await;
        never_validated()
    });
    let mut validation = Some(PendingLoadValidation {
        request_id: 12,
        file_generation: candidate,
        task,
        resume: resume::ResumeLoad::RestoreOwned(recovery_request(3_600.25, false)),
        source_context: super::super::super::MediaSourceContext::OnDemand,
    });
    let mut backlog = VecDeque::new();
    let mut flight = None;
    accept_actor_command(
        &mut state,
        PlayerCmd::interactive_seek(900.0),
        &mut validation,
        &mut backlog,
        &mut flight,
    );
    assert!(
        validation.is_none(),
        "seek must alias onto the current file, not wait for a loadfile that was never sent"
    );
    assert!(command_ready_for_dispatch(
        &state,
        backlog.front().expect("superseding seek remains queued")
    ));
}
