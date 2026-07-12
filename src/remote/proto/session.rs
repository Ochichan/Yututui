//! Protocol v8 session mode: long-lived duplex connections with server push.
//!
//! Wire spec: docs/gui/02 §4–§7. One listener, two connection modes — the first line of
//! a connection either parses as a legacy one-shot [`super::RemoteRequest`] (`command`
//! key) or as a [`HelloRequest`] (`hello` key); the two are structurally unambiguous.
//! After a successful Hello the connection stays open: the client writes [`ClientFrame`]
//! lines, the server writes [`ServerFrame`] lines (correlated replies interleaved with
//! subscribed push events).
//!
//! Semantics pinned here (enforced by the server/session layer):
//! - Subscribing emits **one initial snapshot event per topic before the `Reply{ok}`**,
//!   so client state is always "snapshot + subsequent events" with no missed window.
//! - `seq` is per-session monotonic across all topics; gaps are impossible — a session
//!   that can't keep up is evicted with `Goodbye { reason: "slow_consumer" }`.
//! - Reconnect rebuilds session state: re-Hello → re-Subscribe → fresh snapshots. There is
//!   no unsolicited event replay. Stable-ID state-changing commands can join a retained owner
//!   outcome, while `Status` and session-scoped `RunSearch` queries execute again.
//! - A command's `Reply` is written before any same-turn `Event`s it caused.

use serde::{Deserialize, Serialize};

use super::model_player::{PlayerModel, QueueModel};
use super::{InstanceMode, RemoteCommand, RemoteResponse};

/// First line of a session-mode connection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HelloRequest {
    /// Lowest is 8 on this path; the one-shot path serves 7.
    pub version: u8,
    /// Same per-run token as one-shot mode, from the [`super::InstanceFile`].
    pub token: String,
    pub hello: HelloBody,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HelloBody {
    /// Client name for logging only (`"yututray"`, `"yututray"`, `"test"`) — never trusted.
    pub client: String,
    /// Lowest protocol version the client can speak on this connection (8).
    pub min_version: u8,
}

/// The server's answer to a [`HelloRequest`], and the session's capability surface.
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(
    feature = "ts-export",
    ts(export, export_to = "gui/src/generated/protocol/")
)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HelloAck {
    pub ok: bool,
    /// The version this session will speak.
    pub version: u8,
    #[cfg_attr(feature = "ts-export", ts(type = "number"))]
    pub session_id: u64,
    /// Live per-owner feature strings (docs/gui/02 §10); mirrors the instance descriptor.
    pub capabilities: Vec<String>,
    pub owner_mode: InstanceMode,
    /// On `!ok`: `"bad_token"` | `"bad_version"` | `"sessions_full"` | `"shutting_down"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// One client line after Hello: a client-monotonic id plus the operation, flattened so
/// the wire form is a single flat object (`{"id":1,"op":"subscribe","topics":[…]}`).
/// (`Eq` gone with [`RemoteCommand`]'s — the GUI settings value is a JSON float.)
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClientFrame {
    /// Client-monotonic; echoed in [`ServerFrame::Reply`] / [`ServerFrame::Pong`].
    pub id: u64,
    /// Stable command identity across response timeouts or reconnects to the same advertised
    /// owner within the current 60-second retention window. Never reuse a mutation identity after
    /// that window or after the descriptor token/owner changes. State-changing outcomes are
    /// retained under this identity; read-only `Status` and `RunSearch` execute again for a fresh
    /// live-session result. Ignored for non-command operations and optional for shipped v8 clients.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    /// Page/WebView lifetime namespace. Additive for shipped v8 clients; legacy peers omit it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub page_id: Option<String>,
    #[serde(flatten)]
    pub op: ClientOp,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum ClientOp {
    Subscribe {
        topics: Vec<Topic>,
    },
    Unsubscribe {
        topics: Vec<Topic>,
    },
    /// The full command surface, identical to one-shot mode (`{"op":"command","cmd":…}`).
    Command(RemoteCommand),
    /// Answered with [`ServerFrame::Pong`] directly on the session task — never routed
    /// through the owner loop.
    Ping,
}

/// One server line: a correlated reply, a subscribed push, a pong, or the goodbye.
// Reply carries the full RemoteResponse inline; frames are built, serialized, and
// dropped one at a time (never stored in bulk), so boxing would buy nothing.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "frame", rename_all = "snake_case")]
pub enum ServerFrame {
    /// Correlated with the triggering [`ClientFrame::id`].
    Reply {
        id: u64,
        resp: RemoteResponse,
    },
    /// A subscribed push. `seq` is per-session monotonic across all topics.
    Event {
        seq: u64,
        topic: Topic,
        event: PushEvent,
    },
    Pong {
        id: u64,
    },
    /// Last frame before the server closes the connection.
    /// `reason`: `"shutting_down"` | `"slow_consumer"` | `"idle_timeout"`.
    Goodbye {
        reason: String,
    },
}

/// A domain channel a session subscribes to (docs/gui/02 §7).
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(
    feature = "ts-export",
    ts(export, export_to = "gui/src/generated/protocol/")
)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Topic {
    Player,
    Queue,
    Lyrics,
    Artwork,
    Library,
    Playlists,
    Search,
    Settings,
    Ai,
    Downloads,
    Transfer,
    Accounts,
    System,
}

impl Topic {
    /// The exact wire string (serde snake_case). Used by the session writer to splice
    /// event envelopes around pre-serialized payloads without re-serializing the enum;
    /// a test pins each string to its serde form.
    pub fn wire_str(self) -> &'static str {
        match self {
            Topic::Player => "player",
            Topic::Queue => "queue",
            Topic::Lyrics => "lyrics",
            Topic::Artwork => "artwork",
            Topic::Library => "library",
            Topic::Playlists => "playlists",
            Topic::Search => "search",
            Topic::Settings => "settings",
            Topic::Ai => "ai",
            Topic::Downloads => "downloads",
            Topic::Transfer => "transfer",
            Topic::Accounts => "accounts",
            Topic::System => "system",
        }
    }

    /// Every topic, for "subscribe to everything" clients and exhaustiveness tests.
    pub const ALL: [Topic; 13] = [
        Topic::Player,
        Topic::Queue,
        Topic::Lyrics,
        Topic::Artwork,
        Topic::Library,
        Topic::Playlists,
        Topic::Search,
        Topic::Settings,
        Topic::Ai,
        Topic::Downloads,
        Topic::Transfer,
        Topic::Accounts,
        Topic::System,
    ];
}

/// The payload of a push [`ServerFrame::Event`]. Internally tagged by `kind` so new
/// event kinds are additive; B1+ milestones extend this enum (lyrics, artwork, library
/// invalidations, ticketed results, …).
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(
    feature = "ts-export",
    ts(export, export_to = "gui/src/generated/protocol/")
)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PushEvent {
    /// Full player state; also the initial snapshot for the `player` topic. A track
    /// advance arrives here (via `queue_pos`) — never as a queue snapshot.
    /// Boxed: the model dwarfs the unit variants, and events travel through queues.
    PlayerSnapshot { model: Box<PlayerModel> },
    /// Full queue contents; pushed on membership/order change only, and as the initial
    /// snapshot for the `queue` topic.
    QueueSnapshot { model: QueueModel },
    /// `system` topic: the owner kind changed (only ever observed across reconnects).
    OwnerChanged { mode: InstanceMode },
    /// `system` topic: the owner is shutting down; a `Goodbye` follows.
    ShuttingDown,
    /// `search` topic: one completed [`RunSearch`](super::RemoteCommand::RunSearch),
    /// grouped per concrete catalog. `ticket` echoes the request so the frontend can
    /// drop stale replies; `source` echoes the requested scope (may be `all`).
    SearchCompleted {
        #[cfg_attr(feature = "ts-export", ts(type = "number"))]
        ticket: u64,
        // Echo the requesting WebView lifetime so a replacement page cannot consume it. This is
        // deliberately not a field doc: ts-rs emits such docs after a comma-space, introducing
        // trailing whitespace into the committed generated binding.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[cfg_attr(feature = "ts-export", ts(optional))]
        page_id: Option<String>,
        query: String,
        source: crate::search_source::SearchSource,
        groups: Vec<SearchGroup>,
    },
    /// `settings` topic: the full settings projection — the initial snapshot and the
    /// push after every accepted mutation (the GUI's pending overlay reconciles on it).
    /// Boxed like the player model: it dwarfs the unit variants.
    SettingsSnapshot {
        model: Box<super::model_settings::SettingsModelV8>,
    },
    /// `lyrics` topic: the current track's lyrics — the initial snapshot for the topic,
    /// a clearing push (empty `lines`) on track change, and the resolved lines when the
    /// fetch completes. `video_id` names the track the lines belong to; empty `lines`
    /// means none found (or none yet).
    LyricsSnapshot {
        video_id: Option<String>,
        lines: Vec<super::model::LyricLineModel>,
    },
    /// `library` topic: a mutation invalidated the client's paged library cache.
    LibraryInvalidated,
    /// `playlists` topic: the retained full playlist summary list.
    PlaylistsSnapshot {
        items: Vec<super::model::PlaylistSummaryModel>,
    },
    /// `downloads` topic: the retained ordered projection of daemon-owned downloads.
    DownloadsSnapshot {
        items: Vec<super::model::DownloadStatusModel>,
    },
    /// `ai` topic: retained daemon-side DJ Gem transcript and current turn state.
    AiState {
        messages: Vec<super::model::AiMessageModel>,
        thinking: bool,
        suggestions: Vec<super::model::TrackModel>,
    },
    /// `ai` topic sibling: which queue rows carry recorded DJ Gem / autoplay pick
    /// provenance — the GUI shows its "why?" affordance exactly on these ids and pulls
    /// the rationale on demand with `fetch_why_gem`.
    WhyGemProvenance { video_ids: Vec<String> },
}

/// One catalog's slice of a completed search: a concrete source (never `all`), its
/// result rows, and a per-source failure string surfaced as a chip (e.g. "jamendo:
/// no client id") — `None` on success.
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(
    feature = "ts-export",
    ts(export, export_to = "gui/src/generated/protocol/")
)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SearchGroup {
    pub source: crate::search_source::SearchSource,
    pub tracks: Vec<super::model::TrackModel>,
    pub error: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::super::model::TrackModel;
    use super::super::model_player::EqModel;
    use super::*;
    use crate::queue::Repeat;
    use crate::search_source::SearchSource;

    #[test]
    fn topic_wire_strings_are_snake_case_and_exhaustive() {
        let expect = [
            "player",
            "queue",
            "lyrics",
            "artwork",
            "library",
            "playlists",
            "search",
            "settings",
            "ai",
            "downloads",
            "transfer",
            "accounts",
            "system",
        ];
        for (topic, want) in Topic::ALL.iter().zip(expect) {
            let got = serde_json::to_string(topic).unwrap();
            assert_eq!(got, format!("\"{want}\""));
            assert_eq!(topic.wire_str(), want, "wire_str must match serde");
            let back: Topic = serde_json::from_str(&got).unwrap();
            assert_eq!(back, *topic);
        }
    }

    #[test]
    fn hello_line_is_unambiguous_against_one_shot() {
        let hello = HelloRequest {
            version: 8,
            token: "tok".to_string(),
            hello: HelloBody {
                client: "test".to_string(),
                min_version: 8,
            },
        };
        let line = serde_json::to_string(&hello).unwrap();
        assert_eq!(
            line,
            r#"{"version":8,"token":"tok","hello":{"client":"test","min_version":8}}"#
        );
        // The discrimination contract (docs/gui/02 §4.3): a Hello line does not parse as
        // a one-shot request (missing `command`), and vice versa (missing `hello`).
        assert!(serde_json::from_str::<super::super::RemoteRequest>(&line).is_err());
        let one_shot = r#"{"version":7,"token":"tok","command":{"cmd":"status"}}"#;
        assert!(serde_json::from_str::<HelloRequest>(one_shot).is_err());
        assert!(serde_json::from_str::<super::super::RemoteRequest>(one_shot).is_ok());
    }

    #[test]
    fn hello_ack_omits_reason_on_success() {
        let ack = HelloAck {
            ok: true,
            version: 8,
            session_id: 3,
            capabilities: vec!["events-v8".to_string()],
            owner_mode: InstanceMode::Daemon,
            reason: None,
        };
        let line = serde_json::to_string(&ack).unwrap();
        assert_eq!(
            line,
            r#"{"ok":true,"version":8,"session_id":3,"capabilities":["events-v8"],"owner_mode":"daemon"}"#
        );
        let back: HelloAck = serde_json::from_str(&line).unwrap();
        assert_eq!(back, ack);
    }

    #[test]
    fn client_frames_are_flat_objects() {
        let sub = ClientFrame {
            id: 1,
            request_id: None,
            page_id: None,
            op: ClientOp::Subscribe {
                topics: vec![Topic::Player, Topic::Queue],
            },
        };
        assert_eq!(
            serde_json::to_string(&sub).unwrap(),
            r#"{"id":1,"op":"subscribe","topics":["player","queue"]}"#
        );

        let cmd = ClientFrame {
            id: 2,
            request_id: None,
            page_id: Some("page-a".to_string()),
            op: ClientOp::Command(RemoteCommand::TogglePause),
        };
        assert_eq!(
            serde_json::to_string(&cmd).unwrap(),
            r#"{"id":2,"page_id":"page-a","op":"command","cmd":"toggle_pause"}"#
        );

        let ping = ClientFrame {
            id: 3,
            request_id: None,
            page_id: None,
            op: ClientOp::Ping,
        };
        assert_eq!(
            serde_json::to_string(&ping).unwrap(),
            r#"{"id":3,"op":"ping"}"#
        );

        for frame in [sub, cmd, ping] {
            let line = serde_json::to_string(&frame).unwrap();
            let back: ClientFrame = serde_json::from_str(&line).unwrap();
            assert_eq!(back, frame, "line: {line}");
        }
    }

    #[test]
    fn server_frames_round_trip_with_frame_tag() {
        let reply = ServerFrame::Reply {
            id: 9,
            resp: RemoteResponse::ok("done".to_string()),
        };
        let line = serde_json::to_string(&reply).unwrap();
        assert!(
            line.starts_with(r#"{"frame":"reply","id":9,"#),
            "got {line}"
        );

        let pong = ServerFrame::Pong { id: 4 };
        assert_eq!(
            serde_json::to_string(&pong).unwrap(),
            r#"{"frame":"pong","id":4}"#
        );

        let goodbye = ServerFrame::Goodbye {
            reason: "slow_consumer".to_string(),
        };
        assert_eq!(
            serde_json::to_string(&goodbye).unwrap(),
            r#"{"frame":"goodbye","reason":"slow_consumer"}"#
        );

        let event = ServerFrame::Event {
            seq: 12,
            topic: Topic::System,
            event: PushEvent::ShuttingDown,
        };
        assert_eq!(
            serde_json::to_string(&event).unwrap(),
            r#"{"frame":"event","seq":12,"topic":"system","event":{"kind":"shutting_down"}}"#
        );

        for frame in [reply, pong, goodbye, event] {
            let line = serde_json::to_string(&frame).unwrap();
            let back: ServerFrame = serde_json::from_str(&line).unwrap();
            assert_eq!(back, frame, "line: {line}");
        }
    }

    #[test]
    fn push_event_snapshots_round_trip() {
        let track = TrackModel {
            video_id: "vid".to_string(),
            title: "Song".to_string(),
            artist: "Artist".to_string(),
            album: None,
            duration_ms: Some(194_000),
            source: SearchSource::Youtube,
            is_local: false,
            downloaded: false,
            favorite: true,
            disliked: false,
            display_title: None,
            display_artist: None,
            artwork: None,
            watch_url: Some("https://music.youtube.com/watch?v=vid".to_string()),
            is_live: false,
        };
        let player = PushEvent::PlayerSnapshot {
            model: Box::new(PlayerModel {
                track: Some(track.clone()),
                paused: false,
                volume: 55,
                speed_tenths: 10,
                elapsed_ms: Some(61_500),
                duration_ms: Some(194_000),
                position_epoch: 4,
                shuffle: true,
                repeat: Repeat::All,
                streaming: false,
                radio_mode: false,
                stream_now_playing: None,
                owner_mode: InstanceMode::StandaloneTui,
                eq: EqModel {
                    preset: "Flat".to_string(),
                    bands: [0.0; 10],
                    normalize: false,
                },
                queue_pos: 2,
                queue_len: 3,
            }),
        };
        let queue = PushEvent::QueueSnapshot {
            model: QueueModel {
                rev: 7,
                items: vec![track],
            },
        };
        for event in [player, queue] {
            let line = serde_json::to_string(&event).unwrap();
            let back: PushEvent = serde_json::from_str(&line).unwrap();
            assert_eq!(back, event, "line: {line}");
        }
    }
}
