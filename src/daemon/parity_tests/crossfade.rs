use std::collections::VecDeque;
use std::path::{Path, PathBuf};

use tokio::sync::{mpsc, oneshot};

use super::*;
use crate::util::delivery::DeliveryReceipt;

struct LocalPair {
    dir: PathBuf,
    first: PathBuf,
    second: PathBuf,
    third: PathBuf,
}

impl LocalPair {
    fn create(name: &str) -> Self {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "yututui-daemon-crossfade-{name}-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("crossfade test directory");
        let first = dir.join("first.flac");
        let second = dir.join("second.flac");
        let third = dir.join("third.flac");
        for path in [&first, &second, &third] {
            std::fs::write(path, b"audio").expect("crossfade test file");
        }
        Self {
            dir,
            first,
            second,
            third,
        }
    }

    fn songs(&self) -> Vec<Song> {
        vec![
            local_song("first", &self.first),
            local_song("second", &self.second),
            local_song("third", &self.third),
        ]
    }
}

impl Drop for LocalPair {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn local_song(id: &str, path: &Path) -> Song {
    let mut song = Song::remote(id, format!("local-{id}"), "artist", "4:00");
    song.local_path = Some(path.to_path_buf());
    song
}

struct Owners {
    app: App,
    engine: DaemonEngine,
    app_player: crate::player::PlayerHandle,
    _app_commands: mpsc::Receiver<crate::player::PlayerCmd>,
    engine_player: mpsc::Receiver<crate::player::PlayerCmd>,
}

async fn owners(
    songs: Vec<Song>,
    setting: crate::crossfade::LocalCrossfade,
    position: f64,
) -> Owners {
    let mut queue = Queue::default();
    queue.set(songs, 0);
    let snapshot = queue.snapshot();
    let config = Config {
        local_crossfade_secs: Some(setting.as_secs_f64()),
        ..Config::default()
    };
    let (mut app, mut engine) = hermetic_pair_from_config(config, snapshot);
    app.audio.overlap_support = crate::crossfade::OverlapSupport::Available;

    let current = app.queue.current().cloned().expect("current local song");
    let load = crate::player::PlaybackLoad::from_destination(
        current
            .playback_destination_checked()
            .expect("local destination"),
        crate::player::MediaSourceContext::OnDemand,
    );
    app.playback.loaded = Some(load.clone());
    app.install_seek_parity_state(&current.video_id, position, 240.0);
    app.playback.paused = false;

    let (app_tx, mut app_rx) = mpsc::channel(32);
    let app_player = crate::player::PlayerHandle::test_handle(app_tx);
    let _ = app_player
        .send(crate::player::PlayerCmd::Load(load))
        .expect("initial App player load");
    let _ = recv_command(&mut app_rx).await;

    let engine_player = engine.install_seek_parity_player(&current.video_id, position, 240.0);
    engine.set_overlap_support_for_test(crate::crossfade::OverlapSupport::Available);
    assert!(
        engine
            .handle_player_event(crate::player::PlayerEvent::Paused(false))
            .await
            .is_empty()
    );
    assert_parity("crossfade setup", &app, &engine);

    Owners {
        app,
        engine,
        app_player,
        _app_commands: app_rx,
        engine_player,
    }
}

fn load_from_app_commands(commands: &[Cmd]) -> crate::player::PlaybackLoad {
    commands
        .iter()
        .flat_map(Cmd::player_commands)
        .find_map(|command| match command {
            crate::player::PlayerCmd::Load(load) => Some(load.clone()),
            _ => None,
        })
        .expect("App Load")
}

fn app_player_commands(commands: &[Cmd]) -> Vec<crate::player::PlayerCmd> {
    commands
        .iter()
        .flat_map(Cmd::player_commands)
        .cloned()
        .collect()
}

fn admit_app(app: &mut App, player: &crate::player::PlayerHandle, commands: Vec<Cmd>) {
    let mut pending = VecDeque::from(commands);
    while let Some(command) = pending.pop_front() {
        if let Cmd::PlayerControl(PlayerControl::Intent(intent)) = command {
            let mut intent = *intent;
            let commands = std::mem::take(&mut intent.commands);
            let _ = player
                .send_batch(commands)
                .expect("App player command admission");
            pending.extend(crate::runtime::player_delivery::settle_player_intent(
                app,
                intent,
                Ok(DeliveryReceipt::Enqueued),
            ));
        }
    }
}

fn app_remote(
    app: &mut App,
    player: &crate::player::PlayerHandle,
    command: RemoteCommand,
) -> (
    crate::remote::proto::RemoteResponse,
    Vec<crate::player::PlayerCmd>,
) {
    let (reply_tx, mut reply_rx) = oneshot::channel();
    let commands = app.update(Msg::Remote(command, reply_tx.into()));
    let player_commands = app_player_commands(&commands);
    admit_app(app, player, commands);
    let response = reply_rx
        .try_recv()
        .expect("App remote reply after player admission");
    (response, player_commands)
}

async fn recv_command(
    receiver: &mut mpsc::Receiver<crate::player::PlayerCmd>,
) -> crate::player::PlayerCmd {
    tokio::time::timeout(std::time::Duration::from_secs(1), receiver.recv())
        .await
        .expect("player command timeout")
        .expect("player command channel closed")
}

fn assert_overlap(load: &crate::player::PlaybackLoad) {
    match load.handoff() {
        crate::crossfade::TrackHandoff::Overlap { fade } => {
            assert!((fade.as_secs_f64() - 1.5).abs() < 1e-9);
        }
        handoff => panic!("expected Overlap, got {handoff:?}"),
    }
}

fn assert_cut(load: &crate::player::PlaybackLoad) {
    assert_eq!(load.handoff(), crate::crossfade::TrackHandoff::Cut);
}

#[tokio::test]
async fn time_pos_overlap_fires_once_and_late_outgoing_eof_is_stale() {
    let files = LocalPair::create("due");
    let mut owners = owners(
        files.songs(),
        crate::crossfade::LocalCrossfade::from_tenths(15),
        0.0,
    )
    .await;
    let epochs = PositionEpochs::capture(&owners.app, &owners.engine);
    let app_outgoing_generation = owners.app_player.current_file_generation();
    let engine_outgoing_generation = owners
        .engine
        .player_file_generation_for_test()
        .expect("the daemon player is loaded");

    let first_app = owners.app.update(PlayerMsg::TimePos(238.6));
    let app_load = load_from_app_commands(&first_app);
    assert_overlap(&app_load);
    assert!(owners.app.playback.overlap_fired);
    assert!(
        app_player_commands(&owners.app.update(PlayerMsg::TimePos(238.7))).is_empty(),
        "the App scheduled a second overlap"
    );

    assert!(
        owners
            .engine
            .handle_player_event(crate::player::PlayerEvent::TimePos(238.6))
            .await
            .is_empty()
    );
    let engine_load = match recv_command(&mut owners.engine_player).await {
        crate::player::PlayerCmd::Load(load) => load,
        _ => panic!("expected daemon Load"),
    };
    assert_overlap(&engine_load);
    // The load clears the duration; restore it so only `overlap_fired` can stop a second advance.
    assert!(
        owners
            .engine
            .handle_player_event(crate::player::PlayerEvent::Duration(Some(240.0)))
            .await
            .is_empty()
    );
    assert!(
        owners
            .engine
            .handle_player_event(crate::player::PlayerEvent::TimePos(238.7))
            .await
            .is_empty()
    );
    assert!(owners.engine_player.try_recv().is_err());
    assert!(
        owners
            .engine
            .handle_player_event(crate::player::PlayerEvent::Duration(None))
            .await
            .is_empty()
    );

    admit_app(&mut owners.app, &owners.app_player, first_app);
    epochs.assert_delta(
        "early local overlap",
        PositionEpochs::capture(&owners.app, &owners.engine),
        1,
    );
    assert_eq!(app_load.handoff(), engine_load.handoff());

    assert!(
        app_player_commands(&owners.app.update(PlayerMsg::TimePos(0.1))).is_empty(),
        "new-track App progress loaded again"
    );
    assert!(
        owners
            .engine
            .handle_player_event(crate::player::PlayerEvent::TimePos(0.1))
            .await
            .is_empty()
    );
    assert_parity("early overlap", &owners.app, &owners.engine);

    let before_late_eof = PositionEpochs::capture(&owners.app, &owners.engine);
    let app_late_eof = crate::player::PlayerEvent::file_scoped(
        app_outgoing_generation,
        crate::player::PlayerEvent::Eof,
    );
    assert!(
        !owners.app_player.event_is_current(&app_late_eof),
        "the App runtime must reject an outgoing EOF after the incoming Load"
    );
    assert!(
        owners
            .engine
            .handle_player_event(crate::player::PlayerEvent::file_scoped(
                engine_outgoing_generation,
                crate::player::PlayerEvent::Eof,
            ))
            .await
            .is_empty()
    );
    assert!(owners.engine_player.try_recv().is_err());
    before_late_eof.assert_delta(
        "late outgoing EOF",
        PositionEpochs::capture(&owners.app, &owners.engine),
        0,
    );
    assert_parity("late outgoing EOF", &owners.app, &owners.engine);
}

#[tokio::test]
async fn manual_skip_inside_the_window_cuts_for_both_owners() {
    let files = LocalPair::create("manual");
    let mut owners = owners(
        files.songs(),
        crate::crossfade::LocalCrossfade::from_tenths(15),
        238.6,
    )
    .await;
    let epochs = PositionEpochs::capture(&owners.app, &owners.engine);

    let (app_response, app_commands) =
        app_remote(&mut owners.app, &owners.app_player, RemoteCommand::Next);
    let (engine_response, shutdown, effects) =
        owners.engine.handle_remote(RemoteCommand::Next).await;
    assert!(app_response.ok && engine_response.ok && !shutdown);
    assert!(effects.is_empty());
    let app_load = app_commands
        .into_iter()
        .find_map(|command| match command {
            crate::player::PlayerCmd::Load(load) => Some(load),
            _ => None,
        })
        .expect("App manual Load");
    let engine_load = match recv_command(&mut owners.engine_player).await {
        crate::player::PlayerCmd::Load(load) => load,
        _ => panic!("expected daemon Load"),
    };
    assert_cut(&app_load);
    assert_cut(&engine_load);
    assert_eq!(app_load.handoff(), engine_load.handoff());
    epochs.assert_delta(
        "manual skip inside overlap window",
        PositionEpochs::capture(&owners.app, &owners.engine),
        1,
    );
    assert_parity("manual skip", &owners.app, &owners.engine);
}

#[tokio::test]
async fn non_local_next_and_unavailable_overlap_both_cut() {
    let files = LocalPair::create("cuts");
    let mut remote_next = Song::remote("remote", "remote", "artist", "4:00");
    remote_next.local_path = None;
    let mut non_local = owners(
        vec![local_song("first", &files.first), remote_next],
        crate::crossfade::LocalCrossfade::from_tenths(15),
        0.0,
    )
    .await;
    let epochs = PositionEpochs::capture(&non_local.app, &non_local.engine);
    let app_commands = non_local.app.update(PlayerMsg::Eof);
    let app_load = load_from_app_commands(&app_commands);
    admit_app(&mut non_local.app, &non_local.app_player, app_commands);
    assert!(
        non_local
            .engine
            .handle_player_event(crate::player::PlayerEvent::Eof)
            .await
            .is_empty()
    );
    let engine_load = match recv_command(&mut non_local.engine_player).await {
        crate::player::PlayerCmd::Load(load) => load,
        _ => panic!("expected daemon Load"),
    };
    assert_cut(&app_load);
    assert_cut(&engine_load);
    assert_eq!(app_load.handoff(), engine_load.handoff());
    epochs.assert_delta(
        "non-local end-of-track cut",
        PositionEpochs::capture(&non_local.app, &non_local.engine),
        1,
    );
    assert_parity("non-local incoming", &non_local.app, &non_local.engine);

    let mut unavailable = owners(
        files.songs(),
        crate::crossfade::LocalCrossfade::from_tenths(15),
        0.0,
    )
    .await;
    let blocker = crate::crossfade::OverlapBlocker::OutputBusy;
    assert!(
        unavailable
            .app
            .update(PlayerMsg::OverlapUnavailable(blocker))
            .is_empty()
    );
    assert!(
        unavailable
            .engine
            .handle_player_event(crate::player::PlayerEvent::OverlapUnavailable(blocker))
            .await
            .is_empty()
    );
    let epochs = PositionEpochs::capture(&unavailable.app, &unavailable.engine);
    let app_commands = unavailable.app.update(PlayerMsg::Eof);
    let app_load = load_from_app_commands(&app_commands);
    admit_app(&mut unavailable.app, &unavailable.app_player, app_commands);
    assert!(
        unavailable
            .engine
            .handle_player_event(crate::player::PlayerEvent::Eof)
            .await
            .is_empty()
    );
    let engine_load = match recv_command(&mut unavailable.engine_player).await {
        crate::player::PlayerCmd::Load(load) => load,
        _ => panic!("expected daemon Load"),
    };
    assert_cut(&app_load);
    assert_cut(&engine_load);
    assert_eq!(app_load.handoff(), engine_load.handoff());
    epochs.assert_delta(
        "unavailable end-of-track cut",
        PositionEpochs::capture(&unavailable.app, &unavailable.engine),
        1,
    );
    assert_parity("unavailable overlap", &unavailable.app, &unavailable.engine);
}

#[tokio::test]
async fn remote_crossfade_toggle_updates_both_players_and_later_handoffs() {
    let files = LocalPair::create("remote-setting");
    let mut owners = owners(files.songs(), crate::crossfade::LocalCrossfade::Off, 0.0).await;

    let enable = RemoteCommand::SetSetting {
        change: RemoteSettingChange::LocalCrossfade { tenths: 15 },
    };
    let epochs = PositionEpochs::capture(&owners.app, &owners.engine);
    let (app_response, app_commands) =
        app_remote(&mut owners.app, &owners.app_player, enable.clone());
    let (engine_response, shutdown, effects) = owners.engine.handle_remote(enable).await;
    assert!(app_response.ok && engine_response.ok && !shutdown);
    assert!(effects.is_empty());
    assert!(
        app_commands
            .iter()
            .any(|command| matches!(command, crate::player::PlayerCmd::SetOverlap(true)))
    );
    assert!(matches!(
        recv_command(&mut owners.engine_player).await,
        crate::player::PlayerCmd::SetOverlap(true)
    ));
    assert_eq!(
        app_response
            .status
            .as_ref()
            .and_then(|status| status.settings.local_crossfade_secs)
            .map(crate::crossfade::CrossfadeSecs::as_secs_f64),
        Some(1.5)
    );
    assert_eq!(
        engine_response
            .status
            .as_ref()
            .and_then(|status| status.settings.local_crossfade_secs)
            .map(crate::crossfade::CrossfadeSecs::as_secs_f64),
        Some(1.5)
    );
    epochs.assert_delta(
        "remote crossfade enable",
        PositionEpochs::capture(&owners.app, &owners.engine),
        0,
    );
    assert_parity("remote crossfade enable", &owners.app, &owners.engine);

    let epochs = PositionEpochs::capture(&owners.app, &owners.engine);
    let app_commands = owners.app.update(PlayerMsg::Eof);
    let app_load = load_from_app_commands(&app_commands);
    admit_app(&mut owners.app, &owners.app_player, app_commands);
    assert!(
        owners
            .engine
            .handle_player_event(crate::player::PlayerEvent::Eof)
            .await
            .is_empty()
    );
    let engine_load = match recv_command(&mut owners.engine_player).await {
        crate::player::PlayerCmd::Load(load) => load,
        _ => panic!("expected daemon Load"),
    };
    assert_overlap(&app_load);
    assert_overlap(&engine_load);
    assert_eq!(app_load.handoff(), engine_load.handoff());
    epochs.assert_delta(
        "remote-enabled end-of-track overlap",
        PositionEpochs::capture(&owners.app, &owners.engine),
        1,
    );

    let disable = RemoteCommand::SetSetting {
        change: RemoteSettingChange::LocalCrossfade { tenths: 0 },
    };
    let epochs = PositionEpochs::capture(&owners.app, &owners.engine);
    let (app_response, app_commands) =
        app_remote(&mut owners.app, &owners.app_player, disable.clone());
    let (engine_response, shutdown, effects) = owners.engine.handle_remote(disable).await;
    assert!(app_response.ok && engine_response.ok && !shutdown);
    assert!(effects.is_empty());
    assert!(
        app_commands
            .iter()
            .any(|command| matches!(command, crate::player::PlayerCmd::SetOverlap(false)))
    );
    assert!(matches!(
        recv_command(&mut owners.engine_player).await,
        crate::player::PlayerCmd::SetOverlap(false)
    ));
    assert!(
        app_response
            .status
            .as_ref()
            .and_then(|status| status.settings.local_crossfade_secs)
            .is_none()
    );
    assert!(
        engine_response
            .status
            .as_ref()
            .and_then(|status| status.settings.local_crossfade_secs)
            .is_none()
    );
    epochs.assert_delta(
        "remote crossfade disable",
        PositionEpochs::capture(&owners.app, &owners.engine),
        0,
    );

    let epochs = PositionEpochs::capture(&owners.app, &owners.engine);
    let app_commands = owners.app.update(PlayerMsg::Eof);
    let app_load = load_from_app_commands(&app_commands);
    admit_app(&mut owners.app, &owners.app_player, app_commands);
    assert!(
        owners
            .engine
            .handle_player_event(crate::player::PlayerEvent::Eof)
            .await
            .is_empty()
    );
    let engine_load = match recv_command(&mut owners.engine_player).await {
        crate::player::PlayerCmd::Load(load) => load,
        _ => panic!("expected daemon Load"),
    };
    assert_cut(&app_load);
    assert_cut(&engine_load);
    assert_eq!(app_load.handoff(), engine_load.handoff());
    epochs.assert_delta(
        "remote-disabled end-of-track cut",
        PositionEpochs::capture(&owners.app, &owners.engine),
        1,
    );
    assert_parity("remote crossfade disable", &owners.app, &owners.engine);
}

#[tokio::test]
async fn out_of_range_remote_crossfade_is_rejected_by_both_owners() {
    let files = LocalPair::create("remote-range");
    let mut owners = owners(files.songs(), crate::crossfade::LocalCrossfade::Off, 0.0).await;
    let command = RemoteCommand::SetSetting {
        change: RemoteSettingChange::LocalCrossfade { tenths: 31 },
    };

    let (app_response, app_commands) =
        app_remote(&mut owners.app, &owners.app_player, command.clone());
    let (engine_response, shutdown, effects) = owners.engine.handle_remote(command).await;

    assert!(!shutdown && effects.is_empty() && app_commands.is_empty());
    assert_eq!(app_response.reason.as_deref(), Some("crossfade_range"));
    assert_eq!(engine_response.reason.as_deref(), Some("crossfade_range"));
    assert!(owners.engine_player.try_recv().is_err());
    assert!(owners.app.audio.local_crossfade.is_off());
    assert_parity("rejected crossfade range", &owners.app, &owners.engine);
}
