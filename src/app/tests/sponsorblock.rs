use super::*;

fn has_skip_intent(cmds: &[Cmd]) -> bool {
    cmds.iter().any(|cmd| {
        matches!(
            cmd,
            Cmd::PlayerControl(PlayerControl::Intent(intent))
                if intent.label == "sponsorblock_skip"
        )
    })
}

fn has_fetch(cmds: &[Cmd], id: &str) -> bool {
    cmds.iter().any(
        |cmd| matches!(cmd, Cmd::Fetch(FetchCmd::SponsorBlock(request)) if request.video_id == id),
    )
}

fn load_current(app: &mut App) -> Vec<Cmd> {
    let song = app.queue.current().cloned();
    let mut cmds = app.load_song(song);
    admit_player_transition(app, &mut cmds);
    cmds
}

#[test]
fn load_requests_segments_only_when_the_setting_is_on() {
    let mut off = App::new(100);
    off.queue.set(songs(2), 0);
    assert!(!has_fetch(&load_current(&mut off), "id0"));

    let mut on = App::new(100);
    on.config.streaming.skip_non_music = Some(true);
    on.queue.set(songs(2), 0);
    assert!(has_fetch(&load_current(&mut on), "id0"));
}

#[test]
fn entering_a_segment_skips_once_and_reports_it() {
    let _guard = crate::i18n::lock_for_test();
    crate::i18n::set_language(crate::i18n::Language::English);
    let mut app = app_playing(2, 0);
    app.install_sponsorblock_parity_state(&[(60.0, 90.0)]);

    for position in [57.0, 58.0, 59.0] {
        assert!(!has_skip_intent(&app.update(PlayerMsg::TimePos(position))));
    }
    let mut cmds = app.update(PlayerMsg::TimePos(60.4));
    assert!(has_skip_intent(&cmds));
    let epoch = app.playback.position_epoch;
    admit_player_transition(&mut app, &mut cmds);
    assert_eq!(app.playback.position_epoch, epoch + 1);
    assert_eq!(
        app.status.text,
        "Skipped a non-music segment (SponsorBlock)"
    );
    assert_eq!(app.playback.time_pos, Some(90.0));

    assert!(!has_skip_intent(&app.update(PlayerMsg::TimePos(61.4))));
}

#[test]
fn nothing_is_skipped_when_the_setting_is_off() {
    let mut app = app_playing(2, 0);
    app.install_sponsorblock_parity_state(&[(60.0, 90.0)]);
    app.config.streaming.skip_non_music = Some(false);

    for position in [58.0, 59.0, 60.4] {
        assert!(!has_skip_intent(&app.update(PlayerMsg::TimePos(position))));
    }
}

#[test]
fn nothing_is_skipped_while_a_transport_recovery_is_active() {
    let mut app = app_playing(2, 0);
    app.install_sponsorblock_parity_state(&[(60.0, 90.0)]);
    app.recorder.transport_recovery_active = true;
    for position in [58.0, 59.0, 60.4] {
        assert!(!has_skip_intent(&app.update(PlayerMsg::TimePos(position))));
    }

    app.recorder.transport_recovery_active = false;
    assert!(has_skip_intent(&app.update(PlayerMsg::TimePos(61.0))));
}

#[test]
fn a_seek_into_a_segment_plays_it() {
    let mut app = app_playing(2, 0);
    app.install_sponsorblock_parity_state(&[(60.0, 90.0)]);
    assert!(!has_skip_intent(&app.update(PlayerMsg::TimePos(10.0))));

    app.bump_position_epoch(PositionEpochReason::Seek);
    assert!(!has_skip_intent(&app.update(PlayerMsg::TimePos(70.0))));
    assert!(!has_skip_intent(&app.update(PlayerMsg::TimePos(71.0))));
}
