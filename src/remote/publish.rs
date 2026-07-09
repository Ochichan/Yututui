//! The v8 Publisher: turns owner-state changes into session push events without ever
//! touching the reducer (docs/gui/02 §14).
//!
//! Both owner hosts call [`Publisher::observe`] once per turn on the owner-loop thread,
//! right next to their existing `media.publish(..)` post-turn observers, passing a
//! [`CoreView`] borrow of current state. Change detection is fingerprint-based and
//! cheap; models are built and serialized only when something changed AND someone is
//! subscribed, and the serialized payload fans out by `Arc` (the per-session envelope is
//! spliced at write time by the session writer).
//!
//! Frozen rule (docs/gui/02 §14, tested here): the 1 Hz `PlayerTimePos` tick while
//! playing changes only `elapsed_ms`, which is deliberately **outside** the player
//! fingerprint — a time-tick turn emits nothing, ever. Clients interpolate.
//!
//! Architecture rule: this module reads core state only through [`CoreView`] — reducer
//! message types stay out of `src/remote` entirely (`scripts/check-architecture.sh`
//! enforces that boundary).

use std::sync::Arc;

use crate::api::Song;
use crate::queue::Queue;

use super::proto::{
    EqModel, InstanceMode, PlayerModel, PushEvent, QueueModel, RemoteResponse, ServerFrame, Topic,
    TrackModel,
};
use super::sessions::{RemoteSessionHub, RemoteSessionRef};

/// A read-only borrow of the owner's core state, constructed fresh by each host per
/// turn. Carries exactly what the B0 models need; later milestones extend it (library,
/// settings, …). `elapsed_ms` is host-interpolated to "now" (the same math the OS media
/// session uses) so a snapshot's position is fresh at emit time.
pub struct CoreView<'a> {
    pub queue: &'a Queue,
    pub paused: bool,
    pub volume: i64,
    pub speed_tenths: u16,
    pub elapsed_ms: Option<u64>,
    pub duration_ms: Option<u64>,
    pub position_epoch: u64,
    pub streaming: bool,
    pub radio_mode: bool,
    pub stream_now_playing: Option<String>,
    pub owner_mode: InstanceMode,
    pub eq_preset: String,
    pub eq_bands: [f64; 10],
    pub eq_normalize: bool,
    /// The live config, projected into the `settings` topic model.
    pub config: &'a crate::config::Config,
    /// The media-art cache's resolved file for the CURRENT track (already gated by the
    /// host — stale art from a previous track never appears here). Rides the player
    /// snapshot so the GUI can fetch `ytm://app/art/<key>`.
    pub artwork: Option<super::proto::ArtworkRef>,
}

/// The player-topic change fingerprint. **`elapsed_ms` is deliberately absent** — see
/// the module docs. `duration_ms` is included (it changes on track load, not per tick).
#[derive(PartialEq, Clone)]
struct PlayerFingerprint {
    video_id: Option<String>,
    paused: bool,
    volume: i64,
    speed_tenths: u16,
    position_epoch: u64,
    duration_ms: Option<u64>,
    shuffle: bool,
    repeat: crate::queue::Repeat,
    streaming: bool,
    radio_mode: bool,
    stream_now_playing: Option<String>,
    eq_preset: String,
    eq_bands: [f64; 10],
    eq_normalize: bool,
    queue_pos: usize,
    queue_len: usize,
    /// Art resolves ~1–2 s AFTER a track starts (async cache fetch); keying on it makes
    /// that arrival its own push, or the GUI would show the placeholder until the next
    /// unrelated change.
    artwork_key: Option<String>,
}

impl PlayerFingerprint {
    fn of(view: &CoreView<'_>) -> Self {
        let (pos, len) = view.queue.position();
        Self {
            video_id: view.queue.current().map(|s| s.video_id.clone()),
            paused: view.paused,
            volume: view.volume,
            speed_tenths: view.speed_tenths,
            position_epoch: view.position_epoch,
            duration_ms: view.duration_ms,
            shuffle: view.queue.shuffle,
            repeat: view.queue.repeat,
            streaming: view.streaming,
            radio_mode: view.radio_mode,
            stream_now_playing: view.stream_now_playing.clone(),
            eq_preset: view.eq_preset.clone(),
            eq_bands: view.eq_bands,
            eq_normalize: view.eq_normalize,
            queue_pos: if len == 0 { 0 } else { pos.saturating_sub(1) },
            queue_len: len,
            artwork_key: view.artwork.as_ref().map(|art| art.key.clone()),
        }
    }
}

/// Post-turn diffing observer hosted by both owner loops.
pub struct Publisher {
    hub: Arc<RemoteSessionHub>,
    last_player: Option<PlayerFingerprint>,
    last_queue_rev: u64,
    /// Serialized settings model (rev 0) from the last turn — the settings fingerprint.
    /// Byte comparison over a ~2 KB projection per event turn is comfortably cheap and
    /// immune to forgotten-field drift, unlike a hand-listed fingerprint struct.
    last_settings: Option<Vec<u8>>,
    settings_rev: u64,
}

impl Publisher {
    pub fn new(hub: Arc<RemoteSessionHub>) -> Self {
        Self {
            hub,
            last_player: None,
            last_queue_rev: 0,
            last_settings: None,
            settings_rev: 0,
        }
    }

    /// Called once per owner-loop turn. O(fingerprint) when nothing changed; builds and
    /// serializes a model only for a changed topic that has subscribers.
    pub fn observe(&mut self, view: &CoreView<'_>) {
        let player_now = PlayerFingerprint::of(view);
        if self.last_player.as_ref() != Some(&player_now) {
            self.last_player = Some(player_now);
            if self.hub.any_subscribed(Topic::Player) {
                let payload = event_payload(&PushEvent::PlayerSnapshot {
                    model: Box::new(player_model(view)),
                });
                self.hub.broadcast(Topic::Player, &payload);
            }
        }

        let queue_rev = view.queue.rev();
        if self.last_queue_rev != queue_rev {
            self.last_queue_rev = queue_rev;
            if self.hub.any_subscribed(Topic::Queue) {
                let payload = event_payload(&PushEvent::QueueSnapshot {
                    model: queue_model(view),
                });
                self.hub.broadcast(Topic::Queue, &payload);
            }
        }

        // Building + serializing the settings model is the most expensive part of a turn
        // (the theme role/preset maps dominate), and it previously ran on *every* observe —
        // i.e. every keypress/mouse/worker event — even with zero Settings subscribers, which
        // is the default standalone config. Gate it on an actual subscriber. `last_settings
        // .is_none()` still primes the baseline exactly once (the first observe, long before
        // any subscriber), so the first real diff is correct. While unsubscribed the rev
        // freezes and no bytes are produced; `handle_subscribe` sends a full *current* snapshot
        // on connect, so a subscriber that joins after a change never misses it (test:
        // `late_settings_subscriber_still_sees_changes`).
        if self.hub.any_subscribed(Topic::Settings) || self.last_settings.is_none() {
            let mut settings_now = settings_model(view, 0);
            let settings_bytes = serde_json::to_vec(&settings_now).unwrap_or_default();
            if self.last_settings.as_deref() != Some(settings_bytes.as_slice()) {
                let primed = self.last_settings.is_some();
                self.last_settings = Some(settings_bytes);
                self.settings_rev += 1;
                // The very first observe primes the baseline (nothing changed yet) —
                // matching how the player/queue baselines behave before any subscriber.
                if primed && self.hub.any_subscribed(Topic::Settings) {
                    settings_now.rev = self.settings_rev;
                    let payload = event_payload(&PushEvent::SettingsSnapshot {
                        model: Box::new(settings_now),
                    });
                    self.hub.broadcast(Topic::Settings, &payload);
                }
            }
        }
    }

    pub fn should_observe(&self, state_changed: bool) -> bool {
        state_changed
            || self.last_player.is_none()
            || self.last_queue_rev == 0
            || self.last_settings.is_none()
            || self.hub.any_subscribed(Topic::Player)
            || self.hub.any_subscribed(Topic::Queue)
            || self.hub.any_subscribed(Topic::Settings)
    }

    /// Owner-lane handler for [`super::server::RemoteEvent::SessionSubscribe`]: record
    /// the subscriptions, emit one initial snapshot per **newly** subscribed topic, then
    /// the `Reply{ok}` — all into this session's queue, in that order (docs/gui/02 §6).
    pub fn handle_subscribe(
        &mut self,
        view: &CoreView<'_>,
        session: &RemoteSessionRef,
        frame_id: u64,
        topics: &[Topic],
    ) {
        for topic in session.subscribe(topics) {
            let payload = match topic {
                Topic::Player => Some(event_payload(&PushEvent::PlayerSnapshot {
                    model: Box::new(player_model(view)),
                })),
                Topic::Queue => Some(event_payload(&PushEvent::QueueSnapshot {
                    model: queue_model(view),
                })),
                Topic::Settings => Some(event_payload(&PushEvent::SettingsSnapshot {
                    model: Box::new(settings_model(view, self.settings_rev)),
                })),
                // Event-only (system, search) or not yet served (B1+ topics):
                // registered, no initial snapshot.
                _ => None,
            };
            if let Some(payload) = payload
                && !self.hub.send_event_to(session, topic, &payload)
            {
                return; // evicted mid-subscribe; the reply would go nowhere
            }
        }
        self.hub.send_raw_to(
            session,
            &ServerFrame::Reply {
                id: frame_id,
                resp: RemoteResponse::ok("subscribed".to_string()),
            },
        );
    }

    /// Fan a completed GUI search out on the `search` topic (one-off event, not a
    /// snapshot — the host loop calls this straight from the api-answer lane).
    pub fn search_completed(
        &self,
        ticket: u64,
        query: &str,
        source: crate::search_source::SearchSource,
        groups: &[crate::api::GuiSearchGroup],
    ) {
        if !self.hub.any_subscribed(Topic::Search) {
            return;
        }
        let payload = event_payload(&PushEvent::SearchCompleted {
            ticket,
            query: query.to_string(),
            source,
            groups: groups
                .iter()
                .map(|group| super::proto::SearchGroup {
                    source: group.source,
                    tracks: group.songs.iter().map(track_model).collect(),
                    error: group.error.clone(),
                })
                .collect(),
        });
        self.hub.broadcast(Topic::Search, &payload);
    }

    /// The owner is exiting: `shutting_down` on the `system` topic for subscribers,
    /// then a `Goodbye` to every session (docs/gui/02 §7).
    pub fn shutting_down(&self) {
        if self.hub.any_subscribed(Topic::System) {
            let payload = event_payload(&PushEvent::ShuttingDown);
            self.hub.broadcast(Topic::System, &payload);
        }
        self.hub.shutdown_all();
    }
}

fn event_payload(event: &PushEvent) -> Arc<Vec<u8>> {
    Arc::new(serde_json::to_vec(event).unwrap_or_else(|_| b"{\"kind\":\"shutting_down\"}".to_vec()))
}

/// Build the wire player model from a view. `pub(crate)` for the App↔Daemon parity
/// harness (docs/gui/10 §4), which compares exactly these projections across hosts.
pub(crate) fn player_model(view: &CoreView<'_>) -> PlayerModel {
    let (pos, len) = view.queue.position();
    PlayerModel {
        track: view.queue.current().map(|song| {
            let mut track = track_model(song);
            track.artwork = view.artwork.clone();
            track
        }),
        paused: view.paused,
        volume: view.volume,
        speed_tenths: view.speed_tenths,
        elapsed_ms: view.elapsed_ms,
        duration_ms: view.duration_ms,
        position_epoch: view.position_epoch,
        shuffle: view.queue.shuffle,
        repeat: view.queue.repeat,
        streaming: view.streaming,
        radio_mode: view.radio_mode,
        stream_now_playing: view.stream_now_playing.clone(),
        owner_mode: view.owner_mode,
        eq: EqModel {
            preset: view.eq_preset.clone(),
            bands: view.eq_bands,
            normalize: view.eq_normalize,
        },
        queue_pos: if len == 0 { 0 } else { pos.saturating_sub(1) },
        queue_len: len,
    }
}

pub(crate) fn queue_model(view: &CoreView<'_>) -> QueueModel {
    QueueModel {
        rev: view.queue.rev(),
        items: view.queue.ordered_iter().map(track_model).collect(),
    }
}

/// Project the live [`Config`](crate::config::Config) into the `settings` wire model.
/// Option defaults mirror the documented config semantics (`gapless: None → on`, …).
pub(crate) fn settings_model(view: &CoreView<'_>, rev: u64) -> super::proto::SettingsModelV8 {
    use super::proto::{
        AnimationsModel, AudioSettingsModel, KeymapSettingsModel, PlaybackSettingsModel,
        SearchSettingsModel, SettingsModelV8, StorageSettingsModel, StreamingSettingsModel,
        ThemePresetModel, ThemeSettingsModel, UiSettingsModel,
    };
    use crate::theme::{ThemePreset, ThemeRole};

    let c = view.config;
    let audio = c.audio.runtime();
    let anim = &c.animations;
    let has_key = std::env::var_os("GEMINI_API_KEY").is_some_and(|v| !v.is_empty())
        || c.gemini_api_key
            .as_deref()
            .is_some_and(|key| !key.trim().is_empty());

    SettingsModelV8 {
        rev,
        playback: PlaybackSettingsModel {
            speed_tenths: view.speed_tenths,
            seek_seconds: c.effective_seek_seconds().round() as u16,
            gapless: c.gapless.unwrap_or(true),
            enqueue_next: c.enqueue_next.unwrap_or(false),
            autoplay_on_start: c.autoplay_on_start.unwrap_or(false),
            mouse_wheel_volume: c.mouse_wheel_volume.unwrap_or(true),
            media_controls: c.media_controls.unwrap_or(true),
            volume: view.volume,
            shuffle: view.queue.shuffle,
            repeat: view.queue.repeat,
        },
        eq: EqModel {
            preset: view.eq_preset.clone(),
            bands: view.eq_bands,
            normalize: view.eq_normalize,
        },
        streaming: StreamingSettingsModel {
            ai_enabled: c.ai_enabled.unwrap_or(true),
            gemini_model: c.gemini_model.api_id().to_string(),
            autoplay: c.autoplay_streaming.unwrap_or(false),
            mode: serde_json::to_value(c.streaming.mode)
                .ok()
                .and_then(|v| v.as_str().map(str::to_string))
                .unwrap_or_else(|| "balanced".to_string()),
            has_gemini_key: has_key,
        },
        search: SearchSettingsModel {
            default_source: c.search.source,
            soundcloud_enabled: c.search.soundcloud,
            audius_enabled: c.search.audius,
            jamendo_enabled: c.search.jamendo,
            internet_archive_enabled: c.search.internet_archive,
            radio_browser_enabled: c.search.radio_browser,
            audius_app_name: c.search.audius_app_name.clone(),
            jamendo_client_id: c.search.jamendo_client_id.clone(),
        },
        ui: UiSettingsModel {
            language: match c.language {
                crate::i18n::Language::English => "en".to_string(),
                crate::i18n::Language::Korean => "ko".to_string(),
            },
            mouse: c.mouse.unwrap_or(true),
            album_art: c.album_art.unwrap_or(false),
            romanized_titles: c.romanized_titles.unwrap_or(false),
        },
        storage: StorageSettingsModel {
            download_dir: c.download_dir.as_ref().map(|p| p.display().to_string()),
            cookies_file: c.cookies_file.as_ref().map(|p| p.display().to_string()),
            download_concurrency: c.download_concurrency.unwrap_or(3) as u32,
        },
        audio: AudioSettingsModel {
            backend: audio.backend.id().to_owned(),
            mpv_output: audio.mpv.output,
            mpv_device: audio.mpv.device,
            mpv_cache_forward: audio.mpv.cache_forward,
            mpv_cache_back: audio.mpv.cache_back,
        },
        animations: AnimationsModel {
            master: anim.master,
            pause_unfocused: anim.pause_unfocused,
            fps: anim.effective_fps(),
            title: anim.title,
            heart: anim.heart,
            seekbar: anim.seekbar,
            spinner: anim.spinner,
            eq_bars: anim.eq_bars,
            controls: anim.controls,
            border: anim.border,
            track_intro: anim.track_intro,
            lyrics: anim.lyrics,
            toast: anim.toast,
            volume_flash: anim.volume_flash,
            like_burst: anim.like_burst,
            seek_flash: anim.seek_flash,
            selection: anim.selection,
            stagger: anim.stagger,
            caret: anim.caret,
            tabs: anim.tabs,
            popup_fade: anim.popup_fade,
            activity: anim.activity,
            about_fx: anim.about_fx,
            visualizer: anim.visualizer,
            rain: anim.rain,
            donut: anim.donut,
            starfield: anim.starfield,
            bounce: anim.bounce,
        },
        theme: ThemeSettingsModel {
            preset: c.theme.preset.clone(),
            roles: ThemeRole::ALL
                .into_iter()
                .map(|role| (role.id().to_string(), c.theme.effective_hex(role)))
                .collect(),
            overrides: c.theme.overrides.clone(),
            background_none: c.theme.is_role_transparent(ThemeRole::Background),
            retro: c.retro_mode,
            presets: ThemePreset::ALL
                .into_iter()
                .map(|preset| ThemePresetModel {
                    name: preset.id().to_string(),
                    label: preset.label().to_string(),
                    swatch: ThemeRole::ALL
                        .into_iter()
                        .take(6)
                        .map(|role| (role.id().to_string(), role.default_hex(preset).to_string()))
                        .collect(),
                })
                .collect(),
        },
        keymap: KeymapSettingsModel {
            bindings: c.keybindings.clone(),
            actions: Vec::new(),
        },
    }
}

/// Project a [`Song`] to the wire track shape. B0 carries what the `Song` itself knows;
/// favorite/disliked/display/artwork enrichment lands with its milestone (B1/B2).
fn track_model(song: &Song) -> TrackModel {
    TrackModel {
        video_id: crate::api::sanitize_provider_id(&song.video_id),
        title: crate::api::sanitize_title(&song.title),
        artist: crate::api::sanitize_artist(&song.artist),
        album: song.album.as_deref().map(crate::api::sanitize_album),
        duration_ms: song_duration_ms(song),
        source: song.source,
        is_local: song.local_path.is_some(),
        downloaded: false,
        favorite: false,
        disliked: false,
        display_title: None,
        display_artist: None,
        artwork: None,
        watch_url: None,
        is_live: song.is_radio_station(),
    }
}

/// `duration_secs` when known, else parse the required display string
/// (`"3:45"`, `"1:02:03"`) — old persisted rows lack the numeric field
/// (docs/gui/02 §11.2 conversion rule).
fn song_duration_ms(song: &Song) -> Option<u64> {
    if let Some(secs) = song.duration_secs {
        return Some(u64::from(secs) * 1000);
    }
    let mut total: u64 = 0;
    let mut any = false;
    for part in song.duration.split(':') {
        let field: u64 = part.trim().parse().ok()?;
        total = total.checked_mul(60)?.checked_add(field)?;
        any = true;
    }
    if any && !song.duration.trim().is_empty() {
        // Guard the final seconds→ms scale too: a colon-free garbage duration string parses
        // into a huge `total` that would overflow `* 1000` (debug panic in the owner loop /
        // wrong value in release). Overflow → treat the duration as unknown.
        total.checked_mul(1000)
    } else {
        None
    }
}

/// Test-only view over a bare queue with fixed transport values — shared with the
/// server's session socket tests, which need an owner-lane stand-in. The config is a
/// leaked default: `CoreView` borrows, tests want a one-liner, and a handful of leaked
/// defaults per test process is free.
#[cfg(test)]
pub(crate) fn test_view(queue: &Queue) -> CoreView<'_> {
    CoreView {
        queue,
        paused: false,
        volume: 55,
        speed_tenths: 10,
        elapsed_ms: Some(1_000),
        duration_ms: Some(10_000),
        position_epoch: 1,
        streaming: false,
        radio_mode: false,
        stream_now_playing: None,
        owner_mode: InstanceMode::StandaloneTui,
        eq_preset: "Flat".to_string(),
        eq_bands: [0.0; 10],
        eq_normalize: false,
        config: Box::leak(Box::new(crate::config::Config::default())),
        artwork: None,
    }
}

#[cfg(test)]
mod tests {
    use super::super::proto::PROTOCOL_VERSION;
    use super::super::sessions::{SessionLine, SessionTuning, test_register};
    use super::*;

    fn view(queue: &Queue) -> CoreView<'_> {
        test_view(queue)
    }

    fn song(id: &str) -> Song {
        Song::remote(id, format!("t-{id}"), "a", "3:45")
    }

    fn drain(rx: &mut tokio::sync::mpsc::Receiver<SessionLine>) -> Vec<SessionLine> {
        let mut out = Vec::new();
        while let Ok(line) = rx.try_recv() {
            out.push(line);
        }
        out
    }

    fn kinds(lines: &[SessionLine]) -> Vec<String> {
        lines
            .iter()
            .map(|line| match line {
                SessionLine::Raw(bytes) => format!(
                    "raw:{}",
                    String::from_utf8_lossy(bytes)
                        .split_once('"')
                        .map(|_| "frame")
                        .unwrap_or("?")
                ),
                SessionLine::Event { topic, .. } => format!("event:{}", topic.wire_str()),
            })
            .collect()
    }

    #[test]
    fn subscribe_emits_snapshots_before_reply_and_only_for_new_topics() {
        let (hub, session, mut rx) = test_register(SessionTuning::default());
        let mut publisher = Publisher::new(hub);
        let mut queue = Queue::default();
        queue.set(vec![song("a"), song("b")], 0);

        publisher.handle_subscribe(
            &view(&queue),
            &session,
            1,
            &[Topic::Player, Topic::Queue, Topic::System],
        );
        let lines = drain(&mut rx);
        assert_eq!(
            kinds(&lines),
            vec!["event:player", "event:queue", "raw:frame"],
            "snapshots strictly precede the reply; system has no snapshot"
        );

        // Duplicate subscribe: idempotent — no second snapshot stream, just the reply.
        publisher.handle_subscribe(&view(&queue), &session, 2, &[Topic::Player]);
        let lines = drain(&mut rx);
        assert_eq!(kinds(&lines), vec!["raw:frame"]);
    }

    #[test]
    fn time_tick_only_turns_emit_nothing_frozen() {
        // THE frozen no-tick rule (docs/gui/02 §14): elapsed_ms is outside the player
        // fingerprint, so a PlayerTimePos-only turn (elapsed advanced, nothing else)
        // must emit zero events. Do not weaken this test; fix the fingerprint instead.
        let (hub, session, mut rx) = test_register(SessionTuning::default());
        let mut publisher = Publisher::new(hub);
        let mut queue = Queue::default();
        queue.set(vec![song("a")], 0);

        // Prime the baseline the way a real host does: observe runs from the first
        // loop turn, long before any subscriber exists.
        publisher.observe(&view(&queue));
        publisher.handle_subscribe(&view(&queue), &session, 1, &[Topic::Player, Topic::Queue]);
        drain(&mut rx);

        let mut v = view(&queue);
        publisher.observe(&v);
        assert!(drain(&mut rx).is_empty(), "no-change turn emits nothing");

        for tick in 0..30 {
            v.elapsed_ms = Some(1_000 + tick * 1_000);
            publisher.observe(&v);
        }
        assert!(
            drain(&mut rx).is_empty(),
            "30 time-tick turns must emit nothing"
        );

        // A real discontinuity still pushes exactly once (and carries fresh elapsed).
        v.paused = true;
        publisher.observe(&v);
        publisher.observe(&v);
        let lines = drain(&mut rx);
        assert_eq!(kinds(&lines), vec!["event:player"], "once per change");
    }

    #[test]
    fn queue_changes_push_queue_snapshots_but_cursor_moves_do_not() {
        let (hub, session, mut rx) = test_register(SessionTuning::default());
        let mut publisher = Publisher::new(hub);
        let mut queue = Queue::default();
        queue.set(vec![song("a"), song("b"), song("c")], 0);

        publisher.observe(&view(&queue));
        publisher.handle_subscribe(&view(&queue), &session, 1, &[Topic::Player, Topic::Queue]);
        drain(&mut rx);

        // Membership change → one queue event (and no player event: fingerprint's
        // queue_len changed too, so actually both. Assert the queue event exists and
        // dedup on a second observe).
        queue.extend(vec![song("d")]);
        publisher.observe(&view(&queue));
        let lines = drain(&mut rx);
        assert!(
            kinds(&lines).contains(&"event:queue".to_string()),
            "membership change pushes a queue snapshot: {:?}",
            kinds(&lines)
        );
        publisher.observe(&view(&queue));
        assert!(drain(&mut rx).is_empty(), "no re-push without changes");

        // Cursor move (track advance): player push only — never a queue snapshot.
        queue.next(false);
        publisher.observe(&view(&queue));
        let lines = drain(&mut rx);
        assert_eq!(
            kinds(&lines),
            vec!["event:player"],
            "a track advance is a small player push, not a queue re-push"
        );
    }

    #[test]
    fn late_settings_subscriber_still_sees_changes() {
        // The settings path in `observe` is gated on an actual Settings subscriber (perf: skip
        // the model build + JSON serialize on every keypress when nobody is listening — the
        // default standalone config). This guards the two properties that gate must not break:
        // (1) a subscriber that connects *after* a settings change still learns the current
        // value, and (2) a change made while subscribed still pushes.
        let (hub, session, mut rx) = test_register(SessionTuning::default());
        let mut publisher = Publisher::new(hub);
        let mut queue = Queue::default();
        queue.set(vec![song("a")], 0);

        // First observe primes the baseline (no subscriber yet), as the real host does.
        publisher.observe(&view(&queue));

        // A setting changes while nobody is subscribed: the gate skips the serialize and pushes
        // nothing, but the change must not be lost to a future subscriber.
        let mut v = view(&queue);
        v.eq_preset = "Rock".to_string();
        publisher.observe(&v);
        assert!(
            drain(&mut rx).is_empty(),
            "no subscriber yet → nothing pushed"
        );

        // Subscriber connects: `handle_subscribe` sends the current settings snapshot.
        publisher.handle_subscribe(&v, &session, 1, &[Topic::Settings]);
        assert!(
            kinds(&drain(&mut rx)).contains(&"event:settings".to_string()),
            "a new Settings subscriber receives the current snapshot despite the serialize gate"
        );

        // A further change while subscribed still pushes a settings snapshot.
        v.eq_preset = "Jazz".to_string();
        publisher.observe(&v);
        assert!(
            kinds(&drain(&mut rx)).contains(&"event:settings".to_string()),
            "a subscribed settings change still pushes"
        );
    }

    #[test]
    fn duration_parses_from_display_string_when_secs_absent() {
        let s = song("a"); // "3:45", no duration_secs
        assert_eq!(song_duration_ms(&s), Some(225_000));
        let mut hms = song("b");
        hms.duration = "1:02:03".to_string();
        assert_eq!(song_duration_ms(&hms), Some(3_723_000));
        let mut none = song("c");
        none.duration = String::new();
        assert_eq!(song_duration_ms(&none), None);
        let mut secs = song("d");
        secs.duration_secs = Some(20);
        assert_eq!(song_duration_ms(&secs), Some(20_000));
        // A colon-free garbage duration parses into a huge value; the seconds→ms scale must
        // not overflow (debug panic / wrong value) — it returns None (unknown) instead.
        let mut huge = song("e");
        huge.duration = "18446744073709552".to_string(); // parses to u64, but *1000 overflows
        assert_eq!(song_duration_ms(&huge), None);
    }

    #[test]
    fn track_model_sanitizes_persisted_metadata() {
        let mut song = Song::remote("id", "title", "artist", "3:45");
        song.video_id = format!(
            "{}\n{}",
            "x".repeat(crate::api::MAX_PROVIDER_ID_CHARS + 20),
            '\u{202e}'
        );
        song.title = format!(
            "{}{}",
            "t".repeat(crate::api::MAX_TITLE_CHARS + 20),
            '\u{202e}'
        );
        song.artist = "a\nb".to_owned();
        song.album = Some(format!(
            "{}{}",
            "z".repeat(crate::api::MAX_ALBUM_CHARS + 20),
            '\u{202e}'
        ));

        let track = track_model(&song);

        assert_eq!(
            track.video_id.chars().count(),
            crate::api::MAX_PROVIDER_ID_CHARS
        );
        assert_eq!(track.title.chars().count(), crate::api::MAX_TITLE_CHARS);
        assert_eq!(track.artist, "ab");
        assert_eq!(
            track.album.as_ref().unwrap().chars().count(),
            crate::api::MAX_ALBUM_CHARS
        );
        assert!(!track.video_id.contains('\u{202e}'));
        assert!(!track.title.contains('\u{202e}'));
        assert!(!track.album.as_ref().unwrap().contains('\u{202e}'));
    }

    #[test]
    fn version_constant_still_v8() {
        // publish.rs is v8-only machinery; a bump above 8 must revisit the snapshots.
        assert_eq!(PROTOCOL_VERSION, 8);
    }
}
