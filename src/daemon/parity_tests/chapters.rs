use tokio::sync::mpsc::Receiver;
use yututui_core::chapters::ChapterDirection;

use crate::app::{App, Msg, PlayerMsg};
use crate::daemon::engine::DaemonEngine;
use crate::player::{Chapter, PlayerCmd, PlayerEvent, SeekPrecision};
use crate::remote::proto::{RemoteCommand, RemoteResponse};

use super::harness::{PositionEpochs, admit_app_player_intents, assert_parity, hermetic_pair};

fn chapter_list() -> Vec<Chapter> {
    [
        ("Intro", 0.0),
        ("Verse", 60.0),
        ("Chorus", 120.0),
        ("Outro", 180.0),
    ]
    .into_iter()
    .map(|(title, start_secs)| Chapter {
        title: title.to_owned(),
        start_secs,
    })
    .collect()
}

async fn deliver(app: &mut App, engine: &mut DaemonEngine, chapters: &[Chapter]) {
    app.update(Msg::Player(PlayerMsg::Chapters(chapters.to_vec())));
    engine
        .handle_player_event(PlayerEvent::Chapters(chapters.to_vec()))
        .await;
}

#[derive(Debug, PartialEq)]
enum Sent {
    Seek(f64, SeekPrecision),
    Other,
}

fn summarize(command: &PlayerCmd) -> Sent {
    match command {
        PlayerCmd::SeekAbsolute { seconds, precision } => Sent::Seek(*seconds, *precision),
        _ => Sent::Other,
    }
}

struct Outcome {
    app_reply: RemoteResponse,
    engine_reply: RemoteResponse,
    app_seeks: Vec<Sent>,
    engine_seeks: Vec<Sent>,
}

async fn jump(
    app: &mut App,
    engine: &mut DaemonEngine,
    engine_player: &mut Receiver<PlayerCmd>,
    direction: ChapterDirection,
) -> Outcome {
    let command = RemoteCommand::Chapter { direction };
    let (reply_tx, mut reply_rx) = tokio::sync::oneshot::channel();
    let commands = app.update(Msg::Remote(command.clone(), reply_tx.into()));
    let app_seeks = commands
        .iter()
        .flat_map(crate::app::Cmd::player_commands)
        .map(summarize)
        .collect();
    admit_app_player_intents(app, commands);
    let app_reply = reply_rx.try_recv().expect("App chapter reply");
    let (engine_reply, shutdown, effects) = engine.handle_remote(command).await;
    assert!(!shutdown);
    assert!(effects.is_empty());
    let mut engine_seeks = Vec::new();
    while let Ok(command) = engine_player.try_recv() {
        engine_seeks.push(summarize(&command));
    }
    Outcome {
        app_reply,
        engine_reply,
        app_seeks,
        engine_seeks,
    }
}

fn assert_one_fast_seek(seeks: &[Sent], expected: f64, owner: &str) {
    assert_eq!(
        seeks,
        [Sent::Seek(expected, SeekPrecision::InteractiveFast)],
        "{owner} must send exactly one fast seek"
    );
}

#[tokio::test]
async fn chapter_next_seeks_to_the_same_boundary_with_one_epoch_in_both_owners() {
    let (mut app, mut engine) = hermetic_pair();
    app.install_seek_parity_state("b", 10.0, 240.0);
    let mut engine_player = engine.install_seek_parity_player("b", 10.0, 240.0);
    deliver(&mut app, &mut engine, &chapter_list()).await;
    assert_parity("chapter list delivered", &app, &engine);
    let epochs = PositionEpochs::capture(&app, &engine);

    let outcome = jump(
        &mut app,
        &mut engine,
        &mut engine_player,
        ChapterDirection::Next,
    )
    .await;

    assert!(outcome.app_reply.ok && outcome.engine_reply.ok);
    assert_eq!(outcome.app_reply.reason, outcome.engine_reply.reason);
    assert_one_fast_seek(&outcome.app_seeks, 60.0, "App");
    assert_one_fast_seek(&outcome.engine_seeks, 60.0, "daemon");
    epochs.assert_delta("chapter next", PositionEpochs::capture(&app, &engine), 1);
    assert_parity("chapter next", &app, &engine);

    let chapter = crate::remote::publish::player_model(&engine.core_view())
        .chapter
        .expect("both owners publish the chapter once chapters are known");
    assert_eq!(
        (chapter.title.as_str(), chapter.index, chapter.count),
        ("Verse", 2, 4)
    );
}

#[tokio::test]
async fn chapter_prev_and_boundary_refusals_match_in_both_owners() {
    let (mut app, mut engine) = hermetic_pair();
    app.install_seek_parity_state("b", 130.0, 240.0);
    let mut engine_player = engine.install_seek_parity_player("b", 130.0, 240.0);
    deliver(&mut app, &mut engine, &chapter_list()).await;

    let outcome = jump(
        &mut app,
        &mut engine,
        &mut engine_player,
        ChapterDirection::Prev,
    )
    .await;
    assert!(outcome.app_reply.ok && outcome.engine_reply.ok);
    assert_one_fast_seek(&outcome.app_seeks, 120.0, "App");
    assert_one_fast_seek(&outcome.engine_seeks, 120.0, "daemon");
    assert_parity("chapter prev", &app, &engine);

    let (mut app, mut engine) = hermetic_pair();
    app.install_seek_parity_state("b", 200.0, 240.0);
    let mut engine_player = engine.install_seek_parity_player("b", 200.0, 240.0);
    deliver(&mut app, &mut engine, &chapter_list()).await;
    let epochs = PositionEpochs::capture(&app, &engine);
    let outcome = jump(
        &mut app,
        &mut engine,
        &mut engine_player,
        ChapterDirection::Next,
    )
    .await;
    for reply in [&outcome.app_reply, &outcome.engine_reply] {
        assert!(!reply.ok);
        assert_eq!(reply.reason.as_deref(), Some("chapter_boundary"));
    }
    assert!(outcome.app_seeks.is_empty() && outcome.engine_seeks.is_empty());
    epochs.assert_delta(
        "chapter boundary",
        PositionEpochs::capture(&app, &engine),
        0,
    );
    assert_parity("chapter boundary", &app, &engine);
}

#[tokio::test]
async fn no_chapters_is_refused_with_the_same_reason_and_no_seek_in_both_owners() {
    let (mut app, mut engine) = hermetic_pair();
    app.install_seek_parity_state("b", 10.0, 240.0);
    let mut engine_player = engine.install_seek_parity_player("b", 10.0, 240.0);
    let epochs = PositionEpochs::capture(&app, &engine);

    for direction in [ChapterDirection::Next, ChapterDirection::Prev] {
        let outcome = jump(&mut app, &mut engine, &mut engine_player, direction).await;
        for reply in [&outcome.app_reply, &outcome.engine_reply] {
            assert!(!reply.ok);
            assert_eq!(reply.reason.as_deref(), Some("no_chapters"));
        }
        assert!(outcome.app_seeks.is_empty() && outcome.engine_seeks.is_empty());
    }
    epochs.assert_delta("no chapters", PositionEpochs::capture(&app, &engine), 0);
    assert_parity("no chapters", &app, &engine);
    assert!(
        crate::remote::publish::player_model(&engine.core_view())
            .chapter
            .is_none()
    );
}

#[tokio::test]
async fn nothing_loaded_is_refused_with_the_same_reason_in_both_owners() {
    let (mut app, mut engine) = hermetic_pair();
    let (_unused_tx, mut engine_player) = tokio::sync::mpsc::channel(1);
    let outcome = jump(
        &mut app,
        &mut engine,
        &mut engine_player,
        ChapterDirection::Next,
    )
    .await;
    for reply in [&outcome.app_reply, &outcome.engine_reply] {
        assert!(!reply.ok);
        assert_eq!(reply.reason.as_deref(), Some("nothing_playing"));
    }
    assert!(outcome.app_seeks.is_empty() && outcome.engine_seeks.is_empty());
}

#[tokio::test]
async fn a_chapter_list_left_from_another_track_reads_as_empty_in_both_owners() {
    let (mut app, mut engine) = hermetic_pair();
    app.install_seek_parity_state("b", 10.0, 240.0);
    let mut engine_player = engine.install_seek_parity_player("b", 10.0, 240.0);
    deliver(&mut app, &mut engine, &chapter_list()).await;

    // The next track has no chapters, so mpv reports no change and the old list stays stored.
    app.install_seek_parity_state("c", 10.0, 240.0);
    let mut engine_player_c = engine.install_seek_parity_player("c", 10.0, 240.0);

    let outcome = jump(
        &mut app,
        &mut engine,
        &mut engine_player_c,
        ChapterDirection::Next,
    )
    .await;
    assert!(!outcome.app_reply.ok);
    assert!(!outcome.engine_reply.ok);
    assert_eq!(outcome.app_reply.reason.as_deref(), Some("no_chapters"));
    assert_eq!(outcome.engine_reply.reason, outcome.app_reply.reason);
    assert!(outcome.app_seeks.is_empty());
    assert!(outcome.engine_seeks.is_empty());
    assert!(engine_player.try_recv().is_err());
    assert!(app.core_view().chapter.is_none());
    assert!(engine.core_view().chapter.is_none());
    assert_parity("stale chapter list", &app, &engine);
}
