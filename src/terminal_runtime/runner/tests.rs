use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::app::{App, Mode, Msg, PlayerMsg};

use super::*;

#[test]
fn uncertain_recorder_inventory_has_an_actionable_startup_status() {
    let _guard = crate::i18n::lock_for_test();
    let report = crate::recorder::job::RecoveryReport {
        admission_uncertain: true,
        warnings: vec!["registry enumeration failed".to_owned()],
        ..Default::default()
    };

    let status = recorder_capacity_blocked_status(&report);

    assert!(status.contains("inventory is uncertain"));
    assert!(status.contains("registry enumeration failed"));
    assert!(!status.contains("0 pending / 0 bytes"));
}

#[test]
fn normal_quit_requests_owner_exit_without_an_external_signal() {
    let mut app = App::new(50);
    let shutdown = player::lifetime::ShutdownLatch::new();

    assert!(!owner_exit_requested(&app, &shutdown));
    app.should_quit = true;
    assert!(owner_exit_requested(&app, &shutdown));
}

#[tokio::test]
async fn quit_during_player_startup_aborts_and_reaps_the_producer() {
    struct DropFlag(Arc<AtomicBool>);

    impl Drop for DropFlag {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    let future_dropped = Arc::new(AtomicBool::new(false));
    let future_completed = Arc::new(AtomicBool::new(false));
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    let drop_flag = Arc::clone(&future_dropped);
    let completed = Arc::clone(&future_completed);
    let mut startup = spawn_player_startup(async move {
        let _drop_flag = DropFlag(drop_flag);
        let _ = started_tx.send(());
        let _ = release_rx.await;
        completed.store(true, Ordering::SeqCst);
        Err("late player startup".to_owned())
    });

    started_rx.await.expect("startup future entered");
    startup.cancel_and_join().await;

    assert!(future_dropped.load(Ordering::SeqCst));
    assert!(!future_completed.load(Ordering::SeqCst));
    assert!(startup.ready_rx.is_none());
    assert!(startup.task.is_none());
    assert!(release_tx.send(()).is_err());
}

#[test]
fn perf_stats_track_draws_and_reset_after_log_window() {
    let mut stats = PerfStats {
        enabled: false,
        last_log: Instant::now() - Duration::from_secs(10),
        frames: 0,
        ime_fast_scrubs: 0,
        draw_total: Duration::ZERO,
        draw_max: Duration::ZERO,
        art_resizes: 0,
    };
    stats.record_draw(Duration::from_millis(7));
    stats.record_art_resize();
    stats.record_ime_fast_scrub();
    assert_eq!(stats.frames, 0);
    assert_eq!(stats.art_resizes, 0);
    assert_eq!(stats.ime_fast_scrubs, 0);

    stats.enabled = true;
    stats.record_draw(Duration::from_millis(7));
    stats.record_draw(Duration::from_millis(11));
    stats.record_art_resize();
    stats.record_ime_fast_scrub();
    assert_eq!(stats.frames, 2);
    assert_eq!(stats.draw_total, Duration::from_millis(18));
    assert_eq!(stats.draw_max, Duration::from_millis(11));
    assert_eq!(stats.art_resizes, 1);
    assert_eq!(stats.ime_fast_scrubs, 1);

    stats.maybe_log(&App::new(100));
    assert_eq!(stats.frames, 0);
    assert_eq!(stats.draw_total, Duration::ZERO);
    assert_eq!(stats.draw_max, Duration::ZERO);
    assert_eq!(stats.art_resizes, 0);
    assert_eq!(stats.ime_fast_scrubs, 0);
}

#[tokio::test]
async fn animation_interval_uses_the_legacy_period_and_skip_policy() {
    assert_eq!(anim_tick_period(0), Duration::from_millis(1000));
    assert_eq!(anim_tick_period(1), Duration::from_millis(1000));
    assert_eq!(anim_tick_period(60), Duration::from_millis(16));
    assert_eq!(anim_tick_period(2_000), Duration::from_millis(1));

    let interval = anim_interval(30);
    assert_eq!(interval.period(), Duration::from_millis(33));
    assert_eq!(
        interval.missed_tick_behavior(),
        MissedTickBehavior::Skip,
        "busy periods must collapse into one delivered tick"
    );

    // A 1 Hz interval whose first deadline is 2.5 s overdue has two missed deadlines behind
    // it. Skip delivers the first poll immediately, then jumps to the next future grid point;
    // Burst would make this second poll immediately ready too.
    let first_due = tokio::time::Instant::now() - Duration::from_millis(2_500);
    let mut delayed = anim_interval_at(first_due, 1);
    assert_eq!(delayed.tick().await, first_due);
    assert!(
        tokio::time::timeout(Duration::from_millis(50), delayed.tick())
            .await
            .is_err(),
        "Skip must drop the overdue backlog instead of replaying a second tick"
    );
}

#[tokio::test]
async fn lyrics_interval_uses_100ms_and_drops_missed_boundaries() {
    let interval = lyrics_interval();
    assert_eq!(interval.period(), Duration::from_millis(100));
    assert_eq!(interval.missed_tick_behavior(), MissedTickBehavior::Skip);
}

#[tokio::test]
async fn delayed_200ms_first_poll_advances_exactly_one_frame() {
    let first_due = tokio::time::Instant::now() - Duration::from_millis(200);
    let mut interval = anim_interval_at(first_due, 30);
    let mut app = App::new(100);

    let delivered_due = interval.tick().await;
    assert_eq!(delivered_due, first_due);
    app.update(Msg::AnimTick);

    assert_eq!(
        app.anim_frame(),
        1,
        "missed deadlines are never batch-applied"
    );
}

fn ambient_animation_app() -> App {
    let mut app = App::new(100);
    app.mode = Mode::Search;
    app.config.animations.master = true;
    app.config.animations.caret = true;
    app.config.animations.fps = 30;
    app
}

#[tokio::test]
async fn inactive_overdue_tick_is_immediate_and_retains_draw_credit() {
    let mut app = ambient_animation_app();
    assert!(app.animation_active());
    assert_eq!(app.animation_draw_fps(), 12);

    // At 30→12 fps, two delivered ticks bank 24/30 credit without redrawing.
    for _ in 0..2 {
        app.dirty = false;
        app.update(Msg::AnimTick);
        assert!(!app.dirty);
    }

    // Construct one already-overdue interval before parking. The exact same object survives
    // both focus transitions and is polled only after reactivation.
    let first_due = tokio::time::Instant::now() - Duration::from_millis(2_500);
    let mut interval = anim_interval_at(first_due, 30);
    app.update(Msg::Focus(false));
    assert!(!app.animation_active());

    // The interval remained overdue while its select branch was guarded. Reactivation polls
    // the same interval and therefore gets exactly one immediate Skip tick on existing state.
    app.update(Msg::Focus(true));
    app.dirty = false;
    let delivered_due = tokio::time::timeout(Duration::from_millis(50), interval.tick())
        .await
        .expect("an overdue interval tick must be immediately ready");
    assert_eq!(delivered_due, first_due);
    app.update(Msg::AnimTick);

    assert_eq!(app.anim_frame(), 3);
    assert!(app.dirty, "retained 24/30 credit makes the third tick draw");
}

#[tokio::test]
async fn reactivation_before_deadline_keeps_the_existing_interval_grid() {
    let mut app = ambient_animation_app();
    // A distant first deadline makes this structural test independent of wall-clock load.
    let first_due = tokio::time::Instant::now() + Duration::from_secs(3_600);
    let mut interval = anim_interval_at(first_due, 30);
    let mut fps = 30;

    app.update(Msg::Focus(false));
    assert!(!sync_animation_interval(&mut app, &mut fps, &mut interval));
    app.update(Msg::Focus(true));
    assert!(!sync_animation_interval(&mut app, &mut fps, &mut interval));
    assert_eq!(interval.period(), Duration::from_millis(33));
    assert_eq!(app.anim_frame(), 0);

    // The same synchronization seam does rebuild on the sole approved trigger.
    app.config.animations.fps = 60;
    assert!(sync_animation_interval(&mut app, &mut fps, &mut interval));
    assert_eq!(fps, 60);
    assert_eq!(interval.period(), Duration::from_millis(16));
}

#[tokio::test]
async fn input_without_a_delivered_timer_does_not_advance_animation() {
    let mut app = ambient_animation_app();
    let before = app.anim_frame();
    // Even an overdue clock has no reducer effect until its `tick()` future is delivered by
    // select. This is the exact ordering that the removed input-time phase sync violated.
    let _undelivered_interval =
        anim_interval_at(tokio::time::Instant::now() - Duration::from_millis(200), 30);

    app.update(Msg::Key(KeyEvent::new(
        KeyCode::Char('x'),
        KeyModifiers::NONE,
    )));

    assert_eq!(
        app.anim_frame(),
        before,
        "input handling must not pre-apply elapsed animation ticks"
    );
}

#[tokio::test]
async fn ime_scrub_clock_retains_the_permanent_origin_period() {
    let mut interval = ime_scrub_interval();
    assert_eq!(IME_SCRUB_PERIOD, Duration::from_millis(80));
    assert_eq!(interval.period(), Duration::from_millis(80));

    // The removed burst stopped after eight ticks. Reaching ten ticks without any event-based
    // re-arming proves the scrub clock remains capable of repainting terminal-owned preedit.
    for _ in 0..10 {
        tokio::time::timeout(Duration::from_millis(250), interval.tick())
            .await
            .expect("permanent IME scrub clock must not expire");
    }
}

#[test]
fn ime_scrub_gate_fails_closed_for_state_and_wall_clock_rendering() {
    assert!(!ime_scrub_state_requires_full_draw(
        false, false, false, false, false
    ));
    assert!(ime_scrub_state_requires_full_draw(
        true, false, false, false, false
    ));
    assert!(ime_scrub_state_requires_full_draw(
        false, true, false, false, false
    ));
    assert!(
        ime_scrub_state_requires_full_draw(false, false, true, false, false),
        "a pending native-image clear must go through the consuming full-draw path"
    );
    assert!(
        ime_scrub_state_requires_full_draw(false, false, false, true, false),
        "animation-active views retain origin's wall-clock full draws"
    );
    assert!(
        ime_scrub_state_requires_full_draw(false, false, false, false, true),
        "live-radio stale-edge rendering retains origin's wall-clock full draws"
    );
}

#[test]
fn reducer_turn_gate_catches_visible_updates_that_do_not_set_dirty() {
    let mut app = App::new(100);
    app.dirty = false;
    let status = crate::update::UpdateStatus {
        current: "1.0.0".to_owned(),
        latest: "v1.0.1".to_owned(),
        available: true,
        first_seen: false,
        method: crate::update::InstallMethod::Cargo,
    };

    let _ = app.update(Msg::UpdateChecked(status));

    assert!(
        !app.dirty,
        "this persistent surface update intentionally skips dirty"
    );
    assert!(app.overlays.update_status.is_some());
    assert!(ime_scrub_requires_full_draw(&app, true));
    assert!(
        !ime_scrub_requires_full_draw(&app, false),
        "after a successful full draw the same stable non-Local state may use the fast path"
    );
}

#[test]
fn only_adjacent_time_and_cache_messages_share_an_owner_turn() {
    let first = Msg::Player(PlayerMsg::TimePos(7.0));
    let mut adjacent = BufferedWorkerEvents::default();
    adjacent.extend([RuntimeEvent::App(Msg::Player(PlayerMsg::CacheTime(Some(
        9.0,
    ))))]);
    assert!(matches!(
        take_adjacent_player_progress_pair(&first, &mut adjacent, |_| true),
        Some(Msg::Player(PlayerMsg::CacheTime(Some(9.0))))
    ));
    assert!(adjacent.pop_front().is_none());

    let mut intervening = BufferedWorkerEvents::default();
    intervening.extend([
        RuntimeEvent::App(Msg::Player(PlayerMsg::Duration(Some(180.0)))),
        RuntimeEvent::App(Msg::Player(PlayerMsg::CacheTime(Some(9.0)))),
    ]);
    assert!(take_adjacent_player_progress_pair(&first, &mut intervening, |_| true).is_none());
    assert!(matches!(
        intervening.pop_front(),
        Some(RuntimeEvent::App(Msg::Player(PlayerMsg::Duration(Some(
            180.0
        )))))
    ));

    let reverse = Msg::Player(PlayerMsg::CacheTime(Some(9.0)));
    let mut adjacent = BufferedWorkerEvents::default();
    adjacent.extend([RuntimeEvent::App(Msg::Player(PlayerMsg::TimePos(7.0)))]);
    assert!(matches!(
        take_adjacent_player_progress_pair(&reverse, &mut adjacent, |_| true),
        Some(Msg::Player(PlayerMsg::TimePos(7.0)))
    ));
}

#[test]
fn progress_turns_skip_media_and_remote_projection_but_keep_scrobble_heartbeat() {
    for first in [
        Msg::Player(PlayerMsg::TimePos(7.0)),
        Msg::Player(PlayerMsg::CacheTime(Some(9.0))),
    ] {
        let plan = ObserverPlan::for_messages(&first, None);
        assert!(!plan.project_state, "progress must skip projection");
        assert!(
            plan.drive_scrobble_heartbeat,
            "progress must retain the 1 Hz scrobble clock"
        );
    }

    let first = Msg::Player(PlayerMsg::TimePos(7.0));
    let paired = Msg::Player(PlayerMsg::CacheTime(Some(9.0)));
    assert_eq!(
        ObserverPlan::for_messages(&first, Some(&paired)),
        ObserverPlan::PROGRESS,
        "coalescing the two clocks must not re-enable projection"
    );

    assert_eq!(
        ObserverPlan::for_messages(&Msg::StatusTick, None),
        ObserverPlan::INERT
    );
    assert_eq!(
        ObserverPlan::for_messages(&Msg::LyricsTick, None),
        ObserverPlan::INERT
    );
    assert_eq!(
        ObserverPlan::for_messages(&Msg::Player(PlayerMsg::Duration(Some(180.0))), None,),
        ObserverPlan::PROJECTED,
        "a real media facet still runs media/remote observers"
    );
}

#[test]
fn draw_cycle_and_transient_error_helpers_are_stable() {
    let mut app = App::new(100);
    finish_draw_cycle(&mut app);
    assert!(!app.dirty);

    let error = std::io::Error::from(std::io::ErrorKind::BrokenPipe);
    assert!(!is_transient_terminal_draw_error(&error));
}
