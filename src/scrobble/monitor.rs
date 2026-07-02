//! The listen state machine: pure, clock-injected, fully unit-testable.
//!
//! Both run loops feed it [`Observation`]s derived from the same [`crate::media::MediaSnapshot`]
//! they publish to the OS media session (~1 Hz while playing via mpv `TimePos`, plus every
//! state-changing event). It computes its own transitions keyed on the track `key` — the
//! media diff is too coarse for scrobbling (it reports `track: true` for a duration-only
//! refresh of the same track) — and emits [`ScrobbleAction`]s at exactly the right moments:
//!
//! - **NowPlaying** once per (re-)armed listen, after 5s of accumulated play (a skip storm
//!   therefore emits zero network traffic).
//! - **Scrobble** the moment accumulated listening crosses `min(duration/2, 4min)` — not at
//!   track end — so the durable queue append happens seconds after the threshold and a
//!   crash/quit mid-listen loses nothing.
//! - **Love** when the current track's like flag flips.
//!
//! Accumulation is wall-clock × rate ("track-seconds"), credited in ≤5s steps: pauses
//! credit nothing, seeks neither add nor reset, and a suspend/sleep gap credits at most
//! one step no matter how long the machine was away.

use std::time::Instant;

use super::service::ScrobbleTrack;

/// Longest interval a single observation may credit. mpv reports ~1 Hz while playing, so
/// anything larger means we were paused-without-events, stalled, or asleep — credit it as
/// one ordinary step instead of trusting the gap.
const MAX_CREDIT_STEP: f64 = 5.0;
/// Accumulated play needed before the now-playing announcement (the skip-storm guard).
const NOW_PLAYING_AFTER: f64 = 5.0;
/// A position discontinuity landing at/below this is a "restart" (repeat-one or
/// seek-to-start); above it is an ordinary seek.
const RESTART_EPSILON: f64 = 5.0;
/// The scrobble threshold caps at 4 minutes regardless of track length (Last.fm rule).
const FOUR_MINUTES: f64 = 240.0;
/// Tracks at/below 30s never scrobble (Last.fm rule).
const MIN_TRACK_LEN: f64 = 30.0;

/// A point-in-time view of playback, derived from `MediaSnapshot` by the caller (which
/// also injects both clocks, keeping this module deterministic under test).
#[derive(Debug, Clone)]
pub struct Observation {
    pub track: Option<ObservedTrack>,
    /// `status == Playing`.
    pub playing: bool,
    /// `status == Stopped` (idle / queue empty).
    pub stopped: bool,
    /// Playback position in seconds as of this observation.
    pub position: f64,
    /// Bumped by the core on every position discontinuity (seek / restart).
    pub position_epoch: u64,
    /// Playback speed multiplier.
    pub rate: f64,
    /// Monotonic observe time (drives accumulation).
    pub at: Instant,
    /// Wall-clock unix seconds (stamps listen starts).
    pub wall_unix: i64,
}

/// The track fields scrobbling cares about, snapshot-derived.
#[derive(Debug, Clone, PartialEq)]
pub struct ObservedTrack {
    /// Stable identity (the queue `video_id`).
    pub key: String,
    pub title: String,
    pub artist: String,
    pub album: Option<String>,
    /// Seconds; `None` until mpv (or the catalog) reports it.
    pub duration: Option<f64>,
    /// Live radio — never scrobbled, never announced.
    pub is_live: bool,
    /// `key` starts with `local:`. Gated by the `local_files` setting.
    pub is_local: bool,
    /// Shareable URL when the track has one.
    pub origin_url: Option<String>,
    /// Current in-app favorite state (drives love sync).
    pub liked: bool,
}

/// What the monitor asks the actor to do. `Scrobble` must be appended to the durable
/// queue before any network attempt — that ordering is the crash-safety story.
#[derive(Debug, Clone, PartialEq)]
pub enum ScrobbleAction {
    NowPlaying(ScrobbleTrack),
    Scrobble(ScrobbleTrack),
    Love {
        artist: String,
        title: String,
        love: bool,
    },
}

/// One armed listen.
struct Listen {
    track: ObservedTrack,
    /// Scrobble timestamp: when this listen STARTED (re-set on repeat-one re-arm).
    started_unix: i64,
    /// Listened track-seconds (wall Δ × rate, credited in capped steps).
    accumulated: f64,
    last_at: Instant,
    /// Whether the *previous* observation was playing — the gate for crediting the gap
    /// that just elapsed.
    was_playing: bool,
    /// The rate in effect during that gap.
    last_rate: f64,
    last_epoch: u64,
    last_liked: bool,
    now_playing_sent: bool,
    scrobbled: bool,
}

impl Listen {
    fn arm(track: ObservedTrack, obs: &Observation) -> Self {
        Self {
            last_liked: track.liked,
            track,
            started_unix: obs.wall_unix,
            accumulated: 0.0,
            last_at: obs.at,
            was_playing: obs.playing,
            last_rate: obs.rate,
            last_epoch: obs.position_epoch,
            now_playing_sent: false,
            scrobbled: false,
        }
    }

    fn scrobble_track(&self) -> ScrobbleTrack {
        ScrobbleTrack {
            key: self.track.key.clone(),
            artist: self.track.artist.clone(),
            title: self.track.title.clone(),
            album: self.track.album.clone(),
            duration_secs: self.track.duration.map(|d| d.round() as u32),
            origin_url: self.track.origin_url.clone(),
            started_unix: self.started_unix,
        }
    }

    /// Track metadata good enough to announce/love: real title + artist, not live, and
    /// local files only when the setting allows. Duration is deliberately NOT required —
    /// mpv reports it within ~1s and the scrobble gate re-checks it anyway.
    fn announceable(&self, local_files_ok: bool) -> bool {
        !self.track.is_live
            && !self.track.title.trim().is_empty()
            && !self.track.artist.trim().is_empty()
            && (!self.track.is_local || local_files_ok)
    }

    /// Fully scrobble-eligible: announceable and longer than 30s.
    fn scrobble_eligible(&self, local_files_ok: bool) -> bool {
        self.announceable(local_files_ok)
            && self
                .track
                .duration
                .is_some_and(|d| d.is_finite() && d > MIN_TRACK_LEN)
    }

    fn threshold(&self) -> Option<f64> {
        self.track
            .duration
            .map(|d| (d / 2.0).min(FOUR_MINUTES))
            .filter(|t| *t > 0.0)
    }
}

/// See the module docs for the full spec.
#[derive(Default)]
pub struct ScrobbleMonitor {
    current: Option<Listen>,
}

impl ScrobbleMonitor {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn observe(&mut self, obs: &Observation, local_files_ok: bool) -> Vec<ScrobbleAction> {
        let mut actions = Vec::new();

        // Phase 1 — credit the gap that just elapsed to the armed listen (whatever
        // happens to it next: even a track we're about to leave earned this time).
        if let Some(l) = self.current.as_mut() {
            if l.was_playing {
                let dt = obs.at.saturating_duration_since(l.last_at).as_secs_f64();
                l.accumulated += dt.min(MAX_CREDIT_STEP) * l.last_rate.clamp(0.0, 4.0);
            }
            l.last_at = obs.at;
            l.last_rate = obs.rate;
        }

        // Phase 2 — transitions, keyed on track identity (our own diff, not media::diff).
        match (&obs.track, obs.stopped) {
            (None, _) | (_, true) => {
                // Track left / went idle. If the threshold was crossed, the Scrobble
                // action already fired at crossing time; an under-threshold listen is
                // discarded silently (correct per the rules — skips are free).
                self.current = None;
            }
            (Some(new_track), false) => {
                match self.current.as_mut() {
                    Some(l) if l.track.key == new_track.key => {
                        // Same track. A like flip is worth reporting before anything else
                        // mutates the listen (uses the freshest metadata below either way).
                        if new_track.liked != l.last_liked {
                            l.last_liked = new_track.liked;
                            if l.announceable(local_files_ok) {
                                actions.push(ScrobbleAction::Love {
                                    artist: new_track.artist.clone(),
                                    title: new_track.title.clone(),
                                    love: new_track.liked,
                                });
                            }
                        }
                        // Metadata refresh in place (duration arriving from mpv, romanized
                        // titles, stream now-playing updates…) — never re-arms.
                        l.track = new_track.clone();

                        // Position discontinuity?
                        if obs.position_epoch != l.last_epoch {
                            l.last_epoch = obs.position_epoch;
                            if obs.position <= RESTART_EPSILON && l.scrobbled {
                                // Repeat-one (or manual restart after a full listen):
                                // the previous loop's scrobble is queued; start counting
                                // the next listen from zero with a fresh timestamp.
                                *l = Listen::arm(new_track.clone(), obs);
                            }
                            // Restart before the threshold, or a plain seek: keep the
                            // listen — accumulated time and the start timestamp survive
                            // (one listen's worth of listening = one scrobble).
                        }
                        l.was_playing = obs.playing;
                    }
                    _ => {
                        // New track (or nothing was armed): arm even while duration is
                        // still unknown — eligibility is evaluated lazily each observe.
                        self.current = Some(Listen::arm(new_track.clone(), obs));
                    }
                }
            }
        }

        // Phases 3+4 — eligibility gates and emissions.
        if let Some(l) = self.current.as_mut() {
            if !l.now_playing_sent
                && l.announceable(local_files_ok)
                && l.accumulated >= NOW_PLAYING_AFTER
            {
                l.now_playing_sent = true;
                actions.push(ScrobbleAction::NowPlaying(l.scrobble_track()));
            }
            if !l.scrobbled
                && l.scrobble_eligible(local_files_ok)
                && l.threshold().is_some_and(|t| l.accumulated >= t)
            {
                l.scrobbled = true;
                actions.push(ScrobbleAction::Scrobble(l.scrobble_track()));
            }
        }

        actions
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn track(key: &str, duration: Option<f64>) -> ObservedTrack {
        ObservedTrack {
            key: key.to_owned(),
            title: format!("title-{key}"),
            artist: "artist".to_owned(),
            album: Some("album".to_owned()),
            duration,
            is_live: false,
            is_local: false,
            origin_url: None,
            liked: false,
        }
    }

    /// A deterministic observation stream: each `tick` advances both clocks by 1s.
    struct Sim {
        monitor: ScrobbleMonitor,
        now: Instant,
        unix: i64,
        epoch: u64,
    }

    impl Sim {
        fn new() -> Self {
            Self {
                monitor: ScrobbleMonitor::new(),
                now: Instant::now(),
                unix: 1_751_400_000,
                epoch: 1,
            }
        }

        fn obs(&self, track: Option<ObservedTrack>, playing: bool, position: f64) -> Observation {
            Observation {
                stopped: track.is_none(),
                track,
                playing,
                position,
                position_epoch: self.epoch,
                rate: 1.0,
                at: self.now,
                wall_unix: self.unix,
            }
        }

        fn advance(&mut self, secs: f64) {
            self.now += Duration::from_secs_f64(secs);
            self.unix += secs as i64;
        }

        /// Observe `t` playing for `secs` seconds at 1 Hz, collecting actions.
        fn play(&mut self, t: &ObservedTrack, secs: u32, start_pos: f64) -> Vec<ScrobbleAction> {
            let mut out = Vec::new();
            for i in 0..=secs {
                out.extend(self.monitor.observe(
                    &self.obs(Some(t.clone()), true, start_pos + f64::from(i)),
                    true,
                ));
                if i < secs {
                    self.advance(1.0);
                }
            }
            out
        }
    }

    fn scrobbles(actions: &[ScrobbleAction]) -> Vec<&ScrobbleTrack> {
        actions
            .iter()
            .filter_map(|a| match a {
                ScrobbleAction::Scrobble(t) => Some(t),
                _ => None,
            })
            .collect()
    }

    fn now_playings(actions: &[ScrobbleAction]) -> usize {
        actions
            .iter()
            .filter(|a| matches!(a, ScrobbleAction::NowPlaying(_)))
            .count()
    }

    #[test]
    fn half_duration_scrobbles_once_with_start_timestamp() {
        let mut sim = Sim::new();
        let started = sim.unix;
        let t = track("a", Some(200.0));
        let actions = sim.play(&t, 150, 0.0);
        let s = scrobbles(&actions);
        assert_eq!(s.len(), 1, "exactly one scrobble per listen");
        assert_eq!(s[0].started_unix, started, "timestamp = listen START");
        assert_eq!(s[0].duration_secs, Some(200));
        assert_eq!(now_playings(&actions), 1);
    }

    #[test]
    fn four_minute_rule_caps_long_tracks() {
        let mut sim = Sim::new();
        let t = track("long", Some(600.0));
        // At 250s accumulated (< 300 = half) the 4-minute rule has already fired.
        let actions = sim.play(&t, 250, 0.0);
        assert_eq!(scrobbles(&actions).len(), 1);
        // And it fired at 240s, not later: re-check by replaying to just below.
        let mut sim = Sim::new();
        let below = sim.play(&t, 239, 0.0);
        assert_eq!(scrobbles(&below).len(), 0);
    }

    #[test]
    fn short_track_never_scrobbles() {
        let mut sim = Sim::new();
        let t = track("short", Some(25.0));
        let actions = sim.play(&t, 25, 0.0);
        assert_eq!(scrobbles(&actions).len(), 0);
        // But it still announces now-playing (it's a real track, just too short to count).
        assert_eq!(now_playings(&actions), 1);
    }

    #[test]
    fn pause_gap_credits_nothing() {
        let mut sim = Sim::new();
        let t = track("p", Some(200.0));
        // 60s playing…
        let a1 = sim.play(&t, 60, 0.0);
        assert_eq!(scrobbles(&a1).len(), 0);
        // …pause for 300s (a paused observation, then a long silent gap)…
        sim.advance(1.0);
        sim.monitor
            .observe(&sim.obs(Some(t.clone()), false, 61.0), true);
        sim.advance(300.0);
        sim.monitor
            .observe(&sim.obs(Some(t.clone()), false, 61.0), true);
        sim.advance(1.0);
        // …resume: 100 - 60 - small = ~40 more seconds needed. After 35s: still nothing.
        let a2 = sim.play(&t, 35, 61.0);
        assert_eq!(scrobbles(&a2).len(), 0, "paused gap must not count");
        // 5 more seconds crosses 100s accumulated.
        let a3 = sim.play(&t, 6, 96.0);
        assert_eq!(scrobbles(&a3).len(), 1);
    }

    #[test]
    fn seek_keeps_accumulation_and_timestamp() {
        let mut sim = Sim::new();
        let started = sim.unix;
        let t = track("s", Some(200.0));
        let a1 = sim.play(&t, 80, 0.0);
        assert_eq!(scrobbles(&a1).len(), 0);
        // Seek forward to 150s (epoch bump, position > epsilon): listen survives.
        sim.epoch += 1;
        sim.advance(1.0);
        let a2 = sim.play(&t, 21, 150.0);
        let s = scrobbles(&a2);
        assert_eq!(s.len(), 1, "80 + ~21 crosses 100s despite the seek");
        assert_eq!(s[0].started_unix, started);

        // Seek-to-start while unscrobbled keeps the same listen too.
        let mut sim = Sim::new();
        let started = sim.unix;
        sim.play(&t, 40, 0.0);
        sim.epoch += 1; // user restarts the track at 40s listened
        sim.advance(1.0);
        let a = sim.play(&t, 61, 0.0);
        let s = scrobbles(&a);
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].started_unix, started, "restart before threshold = same listen");
    }

    #[test]
    fn repeat_one_rearms_and_scrobbles_each_loop() {
        let mut sim = Sim::new();
        let t = track("r", Some(100.0));
        let first = sim.play(&t, 99, 0.0);
        assert_eq!(scrobbles(&first).len(), 1);
        let first_ts = scrobbles(&first)[0].started_unix;
        // mpv loops: epoch bump, position back to ~0, same track, already scrobbled.
        sim.epoch += 1;
        sim.advance(1.0);
        let second = sim.play(&t, 99, 0.0);
        let s = scrobbles(&second);
        assert_eq!(s.len(), 1, "second loop scrobbles again");
        assert!(s[0].started_unix > first_ts, "fresh listen, fresh timestamp");
        // And now-playing re-announces on the new loop.
        assert_eq!(now_playings(&second), 1);
    }

    #[test]
    fn skip_storm_is_completely_silent() {
        let mut sim = Sim::new();
        let mut actions = Vec::new();
        for i in 0..5 {
            let t = track(&format!("skip-{i}"), Some(200.0));
            actions.extend(sim.play(&t, 3, 0.0));
            sim.advance(1.0);
        }
        assert!(actions.is_empty(), "sub-5s listens emit nothing at all");
    }

    #[test]
    fn live_streams_and_short_metadata_are_ineligible() {
        let mut sim = Sim::new();
        let mut live = track("radio", None);
        live.is_live = true;
        let actions = sim.play(&live, 400, 0.0);
        assert!(actions.is_empty(), "live: no scrobble, no now-playing");

        // Duration-less non-live track: announces but never scrobbles.
        let mut sim = Sim::new();
        let t = track("nodur", None);
        let actions = sim.play(&t, 400, 0.0);
        assert_eq!(now_playings(&actions), 1);
        assert_eq!(scrobbles(&actions).len(), 0);

        // Empty artist: nothing.
        let mut sim = Sim::new();
        let mut anon = track("anon", Some(200.0));
        anon.artist = String::new();
        assert!(sim.play(&anon, 150, 0.0).is_empty());
    }

    #[test]
    fn local_files_gate() {
        let t = {
            let mut t = track("local:x", Some(200.0));
            t.is_local = true;
            t
        };
        // Gate off: silent.
        let mut sim = Sim::new();
        let mut all = Vec::new();
        for i in 0..=150 {
            all.extend(sim.monitor.observe(
                &sim.obs(Some(t.clone()), true, f64::from(i)),
                false, // local_files_ok = false
            ));
            sim.advance(1.0);
        }
        assert!(all.is_empty());
        // Gate on: scrobbles like any track.
        let mut sim = Sim::new();
        let actions = sim.play(&t, 150, 0.0);
        assert_eq!(scrobbles(&actions).len(), 1);
    }

    #[test]
    fn sleep_gap_is_capped_to_one_step() {
        let mut sim = Sim::new();
        let t = track("z", Some(7200.0));
        sim.monitor
            .observe(&sim.obs(Some(t.clone()), true, 0.0), true);
        // Machine sleeps 2 hours mid-play; one observation arrives after wake.
        sim.advance(7200.0);
        let actions = sim
            .monitor
            .observe(&sim.obs(Some(t.clone()), true, 3.0), true);
        assert!(scrobbles(&actions).is_empty(), "a 2h gap credits ≤5s");
        // 5s credited (< NOW_PLAYING_AFTER not yet reached? exactly 5.0 → announced).
        assert_eq!(now_playings(&actions), 1);
    }

    #[test]
    fn double_speed_scrobbles_at_half_wall_time() {
        let mut sim = Sim::new();
        let t = track("fast", Some(200.0));
        let mut all = Vec::new();
        for i in 0..=51 {
            let mut o = sim.obs(Some(t.clone()), true, f64::from(i) * 2.0);
            o.rate = 2.0;
            all.extend(sim.monitor.observe(&o, true));
            sim.advance(1.0);
        }
        assert_eq!(
            scrobbles(&all).len(),
            1,
            "2× rate: 100 track-seconds in ~50 wall seconds"
        );
    }

    #[test]
    fn love_flip_emits_once_per_flip() {
        let mut sim = Sim::new();
        let t = track("l", Some(200.0));
        sim.play(&t, 10, 0.0);
        sim.advance(1.0);
        let mut liked = t.clone();
        liked.liked = true;
        let a1 = sim
            .monitor
            .observe(&sim.obs(Some(liked.clone()), true, 11.0), true);
        assert!(
            a1.contains(&ScrobbleAction::Love {
                artist: "artist".to_owned(),
                title: "title-l".to_owned(),
                love: true,
            }),
            "like → love"
        );
        // Steady state: no repeat.
        sim.advance(1.0);
        let a2 = sim
            .monitor
            .observe(&sim.obs(Some(liked.clone()), true, 12.0), true);
        assert!(!a2.iter().any(|a| matches!(a, ScrobbleAction::Love { .. })));
        // Unlike → unlove.
        sim.advance(1.0);
        let a3 = sim.monitor.observe(&sim.obs(Some(t.clone()), true, 13.0), true);
        assert!(a3.iter().any(
            |a| matches!(a, ScrobbleAction::Love { love: false, .. })
        ));
    }

    #[test]
    fn late_duration_still_scrobbles() {
        // Catalog had no duration; mpv reports it at t=1s. The listen must not re-arm.
        let mut sim = Sim::new();
        let started = sim.unix;
        let nodur = track("late", None);
        sim.monitor
            .observe(&sim.obs(Some(nodur.clone()), true, 0.0), true);
        sim.advance(1.0);
        let withdur = track("late", Some(120.0));
        let mut all = Vec::new();
        for i in 1..=61 {
            all.extend(sim.monitor.observe(
                &sim.obs(Some(withdur.clone()), true, f64::from(i)),
                true,
            ));
            sim.advance(1.0);
        }
        let s = scrobbles(&all);
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].started_unix, started, "duration refresh didn't re-arm");
    }
}
