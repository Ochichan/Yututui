//! The v8 Publisher: turns owner-state changes into session push events without ever
//! touching the reducer (docs/gui/02 §14).
//!
//! Both owner hosts call [`Publisher::observe`] after turns that can change a projected
//! facet, on the owner-loop thread next to their existing `media.publish(..)` observers,
//! passing a [`CoreView`] borrow of current state. Ordinary position/cache clocks bypass
//! the view entirely because elapsed is deliberately not pushed. Change detection is
//! fingerprint-based; models are built and serialized only when something changed AND
//! someone is subscribed, and the serialized payload fans out by `Arc` (the per-session
//! envelope is spliced at write time by the session writer).
//!
//! Frozen rule (docs/gui/02 §14, tested here): the 1 Hz `PlayerTimePos` tick while
//! playing changes only `elapsed_ms`, which is deliberately **outside** the player
//! fingerprint — a time-tick turn emits nothing, ever. Clients interpolate.
//!
//! Architecture rule: this module reads core state only through [`CoreView`] — reducer
//! message types stay out of `src/remote` entirely (`scripts/check-architecture.sh`
//! enforces that boundary).

use std::borrow::Cow;
use std::path::Path;
use std::sync::Arc;

use crate::api::Song;
use crate::queue::Queue;

use super::proto::{
    EqModel, InstanceMode, PlayerModel, PlaylistSummaryModel, PushEvent, QueueModel,
    RemoteResponse, ServerFrame, Topic, TrackModel,
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
    pub stream_now_playing: Option<Cow<'a, str>>,
    pub owner_mode: InstanceMode,
    pub eq_preset: &'a str,
    pub eq_bands: [f64; 10],
    pub eq_normalize: bool,
    /// The live config, projected into the `settings` topic model.
    pub config: &'a crate::config::Config,
    /// The media-art cache's resolved file for the CURRENT track (already gated by the
    /// host — stale art from a previous track never appears here). Rides the player
    /// snapshot so the GUI can fetch `ytm://app/art/<key>`.
    pub artwork: Option<CoreArtwork<'a>>,
    /// Library favorite membership and dislike signals — the two halves the rating
    /// cycle is synthesized from, projected into every [`TrackModel`]
    /// (docs/gui/02 §11.2). Both owners hold these stores natively.
    pub library: &'a crate::library::Library,
    pub signals: &'a crate::signals::Signals,
}

/// The two rating stores every [`TrackModel`] projection resolves against — the
/// borrowed pair hosts hand to [`Publisher::search_completed`].
pub struct RatingStores<'a> {
    pub library: &'a crate::library::Library,
    pub signals: &'a crate::signals::Signals,
}

/// Borrowed current-art projection. It is converted to the owned wire type only when a player
/// snapshot is actually emitted, rather than cloning the key/path on every observer turn.
pub struct CoreArtwork<'a> {
    pub key: &'a str,
    pub path: Option<&'a Path>,
    pub mime: Option<&'a str>,
}

impl CoreArtwork<'_> {
    fn to_wire(&self) -> super::proto::ArtworkRef {
        super::proto::ArtworkRef {
            key: self.key.to_owned(),
            path: self.path.map(|path| path.to_string_lossy().into_owned()),
            mime: self.mime.map(str::to_owned),
        }
    }
}

/// The player-topic change fingerprint. **`elapsed_ms` is deliberately absent** — see
/// the module docs. `duration_ms` is included (it changes on track load, not per tick).
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
    /// Art resolves ~1–2 s after track start, so its arrival must produce its own push.
    artwork_key: Option<String>,
    /// The CURRENT track's rating halves: a `rate` mutation changes neither the queue
    /// revision nor any other player facet, so the flags themselves must re-push.
    /// (Other rows' flags refresh with the next queue snapshot — the GUI's rating
    /// affordance binds the player model's current track.)
    favorite: bool,
    disliked: bool,
}

impl PlayerFingerprint {
    fn of(view: &CoreView<'_>) -> Self {
        let (pos, len) = view.queue.position();
        Self {
            video_id: view.queue.current().map(|song| song.video_id.clone()),
            paused: view.paused,
            volume: view.volume,
            speed_tenths: view.speed_tenths,
            position_epoch: view.position_epoch,
            duration_ms: view.duration_ms,
            shuffle: view.queue.shuffle,
            repeat: view.queue.repeat,
            streaming: view.streaming,
            radio_mode: view.radio_mode,
            stream_now_playing: view.stream_now_playing.as_deref().map(str::to_owned),
            eq_preset: view.eq_preset.to_owned(),
            eq_bands: view.eq_bands,
            eq_normalize: view.eq_normalize,
            queue_pos: if len == 0 { 0 } else { pos.saturating_sub(1) },
            queue_len: len,
            artwork_key: view.artwork.as_ref().map(|art| art.key.to_owned()),
            favorite: view
                .queue
                .current()
                .is_some_and(|song| view.library.is_favorite(&song.video_id)),
            disliked: view
                .queue
                .current()
                .is_some_and(|song| view.signals.is_disliked(&song.video_id)),
        }
    }

    /// Compare the current borrowed projection against the retained exact snapshot. Owned strings
    /// are allocated only after a real change; unchanged observer turns stay allocation-free
    /// without relying on a lossy hash that could suppress a required remote update.
    fn matches(&self, view: &CoreView<'_>) -> bool {
        let (pos, len) = view.queue.position();
        self.video_id.as_deref() == view.queue.current().map(|song| song.video_id.as_str())
            && self.paused == view.paused
            && self.volume == view.volume
            && self.speed_tenths == view.speed_tenths
            && self.position_epoch == view.position_epoch
            && self.duration_ms == view.duration_ms
            && self.shuffle == view.queue.shuffle
            && self.repeat == view.queue.repeat
            && self.streaming == view.streaming
            && self.radio_mode == view.radio_mode
            && self.stream_now_playing.as_deref() == view.stream_now_playing.as_deref()
            && self.eq_preset == view.eq_preset
            && self.eq_bands == view.eq_bands
            && self.eq_normalize == view.eq_normalize
            && self.queue_pos == if len == 0 { 0 } else { pos.saturating_sub(1) }
            && self.queue_len == len
            && self.artwork_key.as_deref() == view.artwork.as_ref().map(|art| art.key)
            && self.favorite
                == view
                    .queue
                    .current()
                    .is_some_and(|song| view.library.is_favorite(&song.video_id))
            && self.disliked
                == view
                    .queue
                    .current()
                    .is_some_and(|song| view.signals.is_disliked(&song.video_id))
    }
}

/// Post-turn diffing observer hosted by both owner loops.
pub struct Publisher {
    hub: Arc<RemoteSessionHub>,
    last_player: Option<PlayerFingerprint>,
    last_queue_rev: Option<u64>,
    /// Serialized settings model (rev 0) from the last turn — the settings fingerprint.
    /// Byte comparison over a ~2 KB projection per event turn is comfortably cheap and
    /// immune to forgotten-field drift, unlike a hand-listed fingerprint struct.
    last_settings: Option<Vec<u8>>,
    settings_rev: u64,
    /// Retained serialized `lyrics_snapshot` payload — the event-driven lyrics lane's
    /// initial-snapshot source for `handle_subscribe`. Unlike player/queue/settings,
    /// lyrics never ride `observe`: the host publishes explicitly on track change and
    /// fetch completion (B1, docs/gui/02 §7).
    last_lyrics: Option<Arc<Vec<u8>>>,
    last_playlists: Option<Arc<Vec<u8>>>,
    #[cfg(test)]
    last_projection_work: ProjectionWork,
}

/// Test-only record of the concrete projection work performed by [`Publisher::observe`].
#[cfg(test)]
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct ProjectionWork {
    player_fingerprint: bool,
    queue_revision: bool,
    settings_model: bool,
}

impl Publisher {
    pub fn new(hub: Arc<RemoteSessionHub>) -> Self {
        Self {
            hub,
            last_player: None,
            last_queue_rev: None,
            last_settings: None,
            settings_rev: 0,
            last_lyrics: None,
            last_playlists: None,
            #[cfg(test)]
            last_projection_work: ProjectionWork::default(),
        }
    }

    /// Called after owner-loop turns selected by each host. Player and queue retain their historical
    /// cheap baseline comparisons even without subscribers, so a later subscription cannot perturb
    /// the established event sequence. Owned models are still built only for changed subscribed
    /// topics, and the substantially more expensive Settings projection remains subscriber-gated.
    pub(crate) fn observe(&mut self, view: &CoreView<'_>) {
        #[cfg(test)]
        let mut work = ProjectionWork::default();

        let player_subscribed = self.hub.any_subscribed(Topic::Player);
        #[cfg(test)]
        {
            work.player_fingerprint = true;
        }
        if self
            .last_player
            .as_ref()
            .is_none_or(|last| !last.matches(view))
        {
            self.last_player = Some(PlayerFingerprint::of(view));
            if player_subscribed {
                let payload = event_payload(&PushEvent::PlayerSnapshot {
                    model: Box::new(player_model(view)),
                });
                self.hub.broadcast(Topic::Player, &payload);
            }
        }

        let queue_subscribed = self.hub.any_subscribed(Topic::Queue);
        #[cfg(test)]
        {
            work.queue_revision = true;
        }
        let queue_rev = view.queue.rev();
        if self.last_queue_rev != Some(queue_rev) {
            self.last_queue_rev = Some(queue_rev);
            if queue_subscribed {
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
        let settings_subscribed = self.hub.any_subscribed(Topic::Settings);
        if settings_subscribed || self.last_settings.is_none() {
            #[cfg(test)]
            {
                work.settings_model = true;
            }
            let mut settings_now = settings_model(view, 0);
            let settings_bytes = serde_json::to_vec(&settings_now).unwrap_or_default();
            if self.last_settings.as_deref() != Some(settings_bytes.as_slice()) {
                let primed = self.last_settings.is_some();
                self.last_settings = Some(settings_bytes);
                self.settings_rev += 1;
                // The very first observe primes the baseline (nothing changed yet) —
                // matching how the player/queue baselines behave before any subscriber.
                if primed && settings_subscribed {
                    settings_now.rev = self.settings_rev;
                    let payload = event_payload(&PushEvent::SettingsSnapshot {
                        model: Box::new(settings_now),
                    });
                    self.hub.broadcast(Topic::Settings, &payload);
                }
            }
        }

        #[cfg(test)]
        {
            self.last_projection_work = work;
        }
    }

    #[cfg(test)]
    fn observe_work(&mut self, view: &CoreView<'_>) -> ProjectionWork {
        self.observe(view);
        self.last_projection_work
    }

    pub fn should_observe(&self, state_changed: bool) -> bool {
        state_changed
            || self.last_player.is_none()
            || self.last_queue_rev.is_none()
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
        page_id: Option<&str>,
        frame_id: u64,
        topics: &[Topic],
    ) -> bool {
        self.handle_subscribe_with_settlement(view, session, page_id, frame_id, topics, None)
    }

    pub(crate) fn handle_tracked_subscribe(
        &mut self,
        view: &CoreView<'_>,
        session: &RemoteSessionRef,
        page_id: Option<&str>,
        frame_id: u64,
        topics: &[Topic],
        settlement: super::WireSettlement,
    ) -> bool {
        self.handle_subscribe_with_settlement(
            view,
            session,
            page_id,
            frame_id,
            topics,
            Some(settlement),
        )
    }

    fn handle_subscribe_with_settlement(
        &mut self,
        view: &CoreView<'_>,
        session: &RemoteSessionRef,
        page_id: Option<&str>,
        frame_id: u64,
        topics: &[Topic],
        settlement: Option<super::WireSettlement>,
    ) -> bool {
        session
            .apply_subscribe(page_id, topics, |new_topics| {
                for &topic in new_topics {
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
                        // Event-driven lane: serve the retained payload (None before the
                        // host's first publish — the client's empty default covers that).
                        Topic::Lyrics => self.last_lyrics.clone(),
                        Topic::Playlists => self.last_playlists.clone(),
                        // Event-only (system, search) or not yet served (B1+ topics):
                        // registered, no initial snapshot.
                        _ => None,
                    };
                    if let Some(payload) = payload
                        && !self.hub.send_event_to(session, topic, &payload)
                    {
                        return false; // evicted mid-subscribe; the reply would go nowhere
                    }
                }
                let reply = ServerFrame::Reply {
                    id: frame_id,
                    resp: RemoteResponse::ok("subscribed".to_string()),
                };
                match settlement {
                    Some(settlement) => self.hub.send_tracked_raw_to(session, &reply, settlement),
                    None => self.hub.send_raw_to(session, &reply),
                }
            })
            .unwrap_or(false)
    }

    /// Settle a subscribe event that was accepted before owner shutdown. Applying an empty topic
    /// set advances only the already-admitted page generation, then emits the correlated failure
    /// reply while the session writer is still live. A superseded page is explicitly retired.
    pub(crate) fn reject_subscribe_for_shutdown(
        &self,
        session: &RemoteSessionRef,
        page_id: Option<&str>,
        frame_id: u64,
        settlement: super::WireSettlement,
    ) -> bool {
        session
            .apply_subscribe(page_id, &[], |_| {
                self.hub.send_tracked_raw_to(
                    session,
                    &ServerFrame::Reply {
                        id: frame_id,
                        resp: RemoteResponse::err("shutting_down"),
                    },
                    settlement,
                )
            })
            .unwrap_or(false)
    }

    /// Whether any session wants lyrics right now — the host gates its lrclib fetches
    /// on this so a headless daemon with no GUI attached never talks to the network.
    pub fn lyrics_subscribed(&self) -> bool {
        self.hub.any_subscribed(Topic::Lyrics)
    }

    /// Publish the current track's lyrics (`lines` empty = cleared / none found). The
    /// payload is retained as the topic's initial snapshot for later subscribers, and
    /// broadcast only when someone is subscribed.
    pub fn publish_lyrics(
        &mut self,
        video_id: Option<String>,
        lines: Vec<super::proto::LyricLineModel>,
    ) {
        let payload = event_payload(&PushEvent::LyricsSnapshot { video_id, lines });
        self.last_lyrics = Some(Arc::clone(&payload));
        if self.hub.any_subscribed(Topic::Lyrics) {
            self.hub.broadcast(Topic::Lyrics, &payload);
        }
    }

    pub fn playlists_subscribed(&self) -> bool {
        self.hub.any_subscribed(Topic::Playlists)
    }

    pub fn publish_playlists(&mut self, items: Vec<PlaylistSummaryModel>) {
        let payload = event_payload(&PushEvent::PlaylistsSnapshot { items });
        self.last_playlists = Some(Arc::clone(&payload));
        if self.hub.any_subscribed(Topic::Playlists) {
            self.hub.broadcast(Topic::Playlists, &payload);
        }
    }

    pub fn publish_library_invalidated(&self) {
        if self.hub.any_subscribed(Topic::Library) {
            let payload = event_payload(&PushEvent::LibraryInvalidated);
            self.hub.broadcast(Topic::Library, &payload);
        }
    }

    /// Fan a completed GUI search out on the `search` topic (one-off event, not a
    /// snapshot — the host loop calls this straight from the api-answer lane).
    pub fn search_completed(
        &self,
        requester: &super::RemoteSessionScope,
        ticket: u64,
        query: &str,
        source: crate::search_source::SearchSource,
        groups: &[crate::api::GuiSearchGroup],
        stores: RatingStores<'_>,
    ) -> bool {
        let Some(session) = requester.session() else {
            return false;
        };
        let payload = event_payload(&PushEvent::SearchCompleted {
            ticket,
            page_id: requester.page_id().map(str::to_owned),
            query: query.to_string(),
            source,
            groups: groups
                .iter()
                .map(|group| super::proto::SearchGroup {
                    source: group.source,
                    tracks: group
                        .songs
                        .iter()
                        .map(|song| gui_search_track_model(song, stores.library, stores.signals))
                        .collect(),
                    error: group.error.clone(),
                })
                .collect(),
        });
        self.hub
            .send_event_to_subscriber(session, requester.page_id(), Topic::Search, &payload)
    }

    /// The owner is exiting: `shutting_down` on the `system` topic for subscribers,
    /// then a `Goodbye` to every session (docs/gui/02 §7).
    pub(crate) fn quiesce_owner_admission(&self) {
        self.hub.quiesce_owner_admission();
    }

    /// Wait until every request accepted before quiesce has either flushed its response or hit
    /// the socket writer's bounded failure path. Returns `false` only on the outer safety budget.
    pub(crate) async fn wait_for_wire_settlements(&self) -> bool {
        self.hub.wait_for_wire_settlements().await
    }

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
            let mut track = track_model(song, view.library, view.signals);
            track.artwork = view.artwork.as_ref().map(CoreArtwork::to_wire);
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
        stream_now_playing: view.stream_now_playing.as_deref().map(str::to_owned),
        owner_mode: view.owner_mode,
        eq: EqModel {
            preset: view.eq_preset.to_owned(),
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
        items: view
            .queue
            .ordered_iter()
            .map(|song| track_model(song, view.library, view.signals))
            .collect(),
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
            preset: view.eq_preset.to_owned(),
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

/// Project a [`Song`] to the wire track shape, with the rating halves resolved from the
/// owner's library/signals stores (docs/gui/02 §11.2); display/romanization enrichment
/// still lands with its milestone (B3).
pub(crate) fn track_model(
    song: &Song,
    library: &crate::library::Library,
    signals: &crate::signals::Signals,
) -> TrackModel {
    TrackModel {
        video_id: crate::api::sanitize_provider_id(&song.video_id),
        title: crate::api::sanitize_title(&song.title),
        artist: crate::api::sanitize_artist(&song.artist),
        album: song.album.as_deref().map(crate::api::sanitize_album),
        duration_ms: song_duration_ms(song),
        source: song.source,
        is_local: song.local_path.is_some(),
        downloaded: false,
        favorite: library.is_favorite(&song.video_id),
        disliked: signals.is_disliked(&song.video_id),
        display_title: None,
        display_artist: None,
        artwork: None,
        watch_url: None,
        is_live: song.is_radio_station(),
    }
}

fn gui_search_track_model(
    song: &Song,
    library: &crate::library::Library,
    signals: &crate::signals::Signals,
) -> TrackModel {
    let mut model = track_model(song, library, signals);
    model.video_id = crate::api::gui_search_row_id(song);
    model
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
        eq_preset: "Flat",
        eq_bands: [0.0; 10],
        eq_normalize: false,
        config: Box::leak(Box::new(crate::config::Config::default())),
        artwork: None,
        library: Box::leak(Box::new(crate::library::Library::default())),
        signals: Box::leak(Box::new(crate::signals::Signals::default())),
    }
}

#[cfg(test)]
mod tests;
