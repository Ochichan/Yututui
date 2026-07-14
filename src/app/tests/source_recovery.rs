use super::*;

const SOURCE_FAILURE: &str = "HTTP error 403 Forbidden while reading signed source";

#[test]
fn actual_mpv_generic_midtrack_failure_admits_one_position_preserving_retry() {
    let mut app = app_playing(2, 0);
    app.update(PlayerMsg::TimePos(3_600.25));
    app.update(PlayerMsg::Paused(true));
    app.prefetch.last_load_prefetched = true;
    let epoch = app.playback.position_epoch;
    let history_len = app.library.history.len();

    let mut commands = app.update(PlayerMsg::Error(
        crate::player::recovery::GENERIC_LOADING_FAILURE.to_owned(),
    ));
    let request = resume_load(&commands).expect("midtrack loading failure uses LoadWithResume");
    assert_eq!(request.position_secs, 3_600.25);
    assert!(request.paused);
    assert_eq!(
        request.source_context,
        crate::player::MediaSourceContext::OnDemand
    );
    assert_eq!(app.playback.time_pos, Some(3_600.25));
    assert_eq!(app.playback.position_epoch, epoch);

    admit_player_transition(&mut app, &mut commands);
    assert_eq!(current(&app), "id0");
    assert_eq!(app.playback.time_pos, Some(3_600.25));
    assert!(app.playback.paused);
    assert_eq!(app.playback.position_epoch, epoch + 1);
    assert_eq!(app.library.history.len(), history_len);
    assert!(!app.prefetch.last_load_prefetched);

    app.update(PlayerMsg::TimePos(3_600.5));
    assert_eq!(app.playback.position_epoch, epoch + 1);

    let mut second = app.update(PlayerMsg::Error(
        crate::player::recovery::GENERIC_LOADING_FAILURE.to_owned(),
    ));
    assert!(resume_load(&second).is_none(), "one retry per logical item");
    admit_player_transition(&mut app, &mut second);
    assert_eq!(
        current(&app),
        "id1",
        "second failure uses the breaker/skip path"
    );
}

#[test]
fn newer_admitted_seek_supersedes_a_prepared_source_retry() {
    let mut app = app_playing(2, 0);
    app.update(PlayerMsg::TimePos(900.0));
    let recovery = app.update(PlayerMsg::Error(SOURCE_FAILURE.to_owned()));
    assert!(resume_load(&recovery).is_some());

    let mut seek = app.update(Msg::Media(crate::media::MediaCommand::SeekTo(120.0)));
    admit_player_transition(&mut app, &mut seek);
    assert_eq!(app.playback.time_pos, Some(120.0));

    assert_rejected_before_send(&mut app, recovery);
    assert_eq!(app.playback.time_pos, Some(120.0));
}

#[test]
fn initial_load_failure_keeps_existing_non_resume_behavior() {
    let mut app = app_playing(2, 0);
    app.playback.time_pos = None;
    app.prefetch.last_load_prefetched = true;

    let commands = app.update(PlayerMsg::Error(
        crate::player::recovery::GENERIC_LOADING_FAILURE.to_owned(),
    ));
    assert!(resume_load(&commands).is_none());
    assert_loads_video(&commands, "id0");
}
