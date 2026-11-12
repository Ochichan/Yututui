use crate::app::{Cmd, Msg, PlayerMsg};
use crate::player::{PlayerCmd, PlayerEvent, SeekPrecision};

use super::harness::{PositionEpochs, admit_app_player_intents, assert_parity, hermetic_pair};

fn exact_seek_to(command: &PlayerCmd, expected: f64) -> bool {
    matches!(
        command,
        PlayerCmd::SeekAbsolute { seconds, precision: SeekPrecision::Exact }
            if (*seconds - expected).abs() < f64::EPSILON
    )
}

const SEGMENT: (f64, f64) = (60.0, 90.0);

/// Report `position` to both owners and return the seek each one sent, if any.
async fn report(
    app: &mut crate::app::App,
    engine: &mut crate::daemon::engine::DaemonEngine,
    engine_player: &mut tokio::sync::mpsc::Receiver<PlayerCmd>,
    position: f64,
) -> (Option<f64>, Option<f64>) {
    let commands = app.update(Msg::Player(PlayerMsg::TimePos(position)));
    let app_seek = commands
        .iter()
        .find_map(Cmd::player_command)
        .map(|command| {
            assert!(
                exact_seek_to(command, SEGMENT.1),
                "App sent the wrong command"
            );
            SEGMENT.1
        });
    admit_app_player_intents(app, commands);
    engine
        .handle_player_event(PlayerEvent::TimePos(position))
        .await;
    let engine_seek = engine_player.try_recv().ok().map(|command| {
        assert!(
            exact_seek_to(&command, SEGMENT.1),
            "daemon sent the wrong command"
        );
        SEGMENT.1
    });
    assert!(
        engine_player.try_recv().is_err(),
        "daemon sent a second command"
    );
    (app_seek, engine_seek)
}

#[tokio::test]
async fn entering_a_non_music_segment_sends_one_exact_seek_and_one_epoch_in_both_owners() {
    let (mut app, mut engine) = hermetic_pair();
    app.install_seek_parity_state("b", 10.0, 180.0);
    let mut engine_player = engine.install_seek_parity_player("b", 10.0, 180.0);
    app.install_sponsorblock_parity_state(&[SEGMENT]);
    engine.install_sponsorblock_parity_state(&[SEGMENT]);

    for position in [57.0, 58.0, 59.0] {
        assert_eq!(
            report(&mut app, &mut engine, &mut engine_player, position).await,
            (None, None)
        );
    }
    let epochs = PositionEpochs::capture(&app, &engine);

    assert_eq!(
        report(&mut app, &mut engine, &mut engine_player, 60.0).await,
        (Some(SEGMENT.1), Some(SEGMENT.1))
    );
    // A position reported before mpv processed the seek must not trigger a second skip.
    assert_eq!(
        report(&mut app, &mut engine, &mut engine_player, 61.0).await,
        (None, None)
    );

    epochs.assert_delta(
        "sponsorblock skip",
        PositionEpochs::capture(&app, &engine),
        1,
    );
    assert_parity("sponsorblock skip", &app, &engine);
}

#[tokio::test]
async fn both_owners_drop_a_result_for_another_track_and_apply_the_loaded_one() {
    let (mut app, mut engine) = hermetic_pair();
    app.install_seek_parity_state("b", 10.0, 180.0);
    let mut engine_player = engine.install_seek_parity_player("b", 10.0, 180.0);
    app.install_sponsorblock_parity_state(&[]);
    engine.install_sponsorblock_parity_state(&[]);
    let segments = crate::sponsorblock::sanitize(&[SEGMENT], None);

    let result = |video_id: &str| crate::sponsorblock::SponsorBlockEvent::Result {
        video_id: video_id.to_owned(),
        segments: segments.clone(),
    };
    app.update(Msg::Player(PlayerMsg::SponsorBlock(result("other"))));
    engine.on_sponsorblock_result("other".to_owned(), segments.clone());
    for position in [58.0, 59.0, 60.0] {
        assert_eq!(
            report(&mut app, &mut engine, &mut engine_player, position).await,
            (None, None),
            "a result for another track must not arm a skip"
        );
    }

    app.update(Msg::Player(PlayerMsg::SponsorBlock(result("b"))));
    engine.on_sponsorblock_result("b".to_owned(), segments.clone());
    assert_eq!(
        report(&mut app, &mut engine, &mut engine_player, 61.0).await,
        (Some(SEGMENT.1), Some(SEGMENT.1))
    );
    assert_parity("sponsorblock late result", &app, &engine);
}

#[tokio::test]
async fn a_seek_that_lands_inside_a_segment_plays_it_in_both_owners() {
    let (mut app, mut engine) = hermetic_pair();
    app.install_seek_parity_state("b", 10.0, 180.0);
    let mut engine_player = engine.install_seek_parity_player("b", 10.0, 180.0);
    app.install_sponsorblock_parity_state(&[SEGMENT]);
    engine.install_sponsorblock_parity_state(&[SEGMENT]);
    for position in [10.0, 11.0] {
        report(&mut app, &mut engine, &mut engine_player, position).await;
    }

    let (reply_tx, _reply_rx) = tokio::sync::oneshot::channel();
    let commands = app.update(Msg::Remote(
        crate::remote::proto::RemoteCommand::SeekTo { ms: 70_000 },
        reply_tx.into(),
    ));
    admit_app_player_intents(&mut app, commands);
    let (reply, _, _) = engine
        .handle_remote(crate::remote::proto::RemoteCommand::SeekTo { ms: 70_000 })
        .await;
    assert!(reply.ok);
    while engine_player.try_recv().is_ok() {}

    // The first report can predate the seek.
    for position in [12.0, 70.0, 71.0, 72.0] {
        assert_eq!(
            report(&mut app, &mut engine, &mut engine_player, position).await,
            (None, None)
        );
    }
    assert_parity("sponsorblock seek into segment", &app, &engine);
}

#[tokio::test]
async fn a_short_seek_into_a_segment_after_two_stale_reports_plays_it_in_both_owners() {
    let (mut app, mut engine) = hermetic_pair();
    app.install_seek_parity_state("b", 55.0, 180.0);
    let mut engine_player = engine.install_seek_parity_player("b", 55.0, 180.0);
    app.install_sponsorblock_parity_state(&[SEGMENT]);
    engine.install_sponsorblock_parity_state(&[SEGMENT]);
    for position in [55.0, 56.0] {
        report(&mut app, &mut engine, &mut engine_player, position).await;
    }

    let (reply_tx, _reply_rx) = tokio::sync::oneshot::channel();
    let commands = app.update(Msg::Remote(
        crate::remote::proto::RemoteCommand::SeekTo { ms: 61_000 },
        reply_tx.into(),
    ));
    admit_app_player_intents(&mut app, commands);
    let (reply, _, _) = engine
        .handle_remote(crate::remote::proto::RemoteCommand::SeekTo { ms: 61_000 })
        .await;
    assert!(reply.ok);
    while engine_player.try_recv().is_ok() {}

    // Two reports that predate the seek, then the landing inside the segment.
    for position in [57.0, 58.0, 61.0, 62.0] {
        assert_eq!(
            report(&mut app, &mut engine, &mut engine_player, position).await,
            (None, None)
        );
    }
    assert_parity("sponsorblock short seek into segment", &app, &engine);
}
