use super::*;

fn reserve_next(state: &mut DispatchState) -> u64 {
    reserve_published_file_generation(state, 0)
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
