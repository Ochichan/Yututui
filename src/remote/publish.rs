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
        }
    }
}

/// Post-turn diffing observer hosted by both owner loops.
pub struct Publisher {
    hub: Arc<RemoteSessionHub>,
    last_player: Option<PlayerFingerprint>,
    last_queue_rev: u64,
}

impl Publisher {
    pub fn new(hub: Arc<RemoteSessionHub>) -> Self {
        Self {
            hub,
            last_player: None,
            last_queue_rev: 0,
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
                // Event-only in B0 (system) or not yet served (B1+ topics): registered,
                // no initial snapshot.
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
        track: view.queue.current().map(track_model),
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

/// Project a [`Song`] to the wire track shape. B0 carries what the `Song` itself knows;
/// favorite/disliked/display/artwork enrichment lands with its milestone (B1/B2).
fn track_model(song: &Song) -> TrackModel {
    TrackModel {
        video_id: song.video_id.clone(),
        title: song.title.clone(),
        artist: song.artist.clone(),
        album: song.album.clone(),
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
        Some(total * 1000)
    } else {
        None
    }
}

/// Test-only view over a bare queue with fixed transport values — shared with the
/// server's session socket tests, which need an owner-lane stand-in.
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
    }

    #[test]
    fn version_constant_still_v8() {
        // publish.rs is v8-only machinery; a bump above 8 must revisit the snapshots.
        assert_eq!(PROTOCOL_VERSION, 8);
    }
}
