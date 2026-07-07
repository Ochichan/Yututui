//! Semantic delivery policies for owner-loop events.

use std::collections::{HashMap, VecDeque};
use std::hash::Hash;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventPolicy {
    MustDeliver { lane: EventLane },
    MustReplyOrBusy { lane: EventLane },
    CoalesceLatest { lane: EventLane, key: EventKey },
    DropIfStale { stale_key: EventKey },
    BestEffort { reason: &'static str },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EventLane {
    Control,
    RemoteCommand,
    WorkResult,
    Telemetry,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EventKey {
    AiThinking,
    AiWork,
    ArtResize,
    ArtworkVideo,
    DownloadProgress,
    DownloadResult,
    GuiSearchTicket,
    LyricsVideo,
    MediaArtVideo,
    PlayerAudioCodec,
    PlayerCacheTime,
    PlayerDuration,
    PlayerFileFormat,
    PlayerMetadata,
    PlayerPaused,
    PlayerTimePos,
    PlayerVolume,
    PlaylistIntent,
    ResolverVideo,
    SearchRequest,
    StreamingSeed,
    ToolProgress,
    ToolResult,
    TransferAuth,
    TransferJob,
    UpdateCheck,
    VideoOverlayGeneration,
    VideoOverlayPaused,
    YtdlpHealVideo,
}

impl EventPolicy {
    pub const fn name(self) -> &'static str {
        match self {
            EventPolicy::MustDeliver { .. } => "must_deliver",
            EventPolicy::MustReplyOrBusy { .. } => "must_reply_or_busy",
            EventPolicy::CoalesceLatest { .. } => "coalesce_latest",
            EventPolicy::DropIfStale { .. } => "drop_if_stale",
            EventPolicy::BestEffort { .. } => "best_effort",
        }
    }

    pub const fn lane(self) -> Option<EventLane> {
        match self {
            EventPolicy::MustDeliver { lane }
            | EventPolicy::MustReplyOrBusy { lane }
            | EventPolicy::CoalesceLatest { lane, .. } => Some(lane),
            EventPolicy::DropIfStale { .. } | EventPolicy::BestEffort { .. } => None,
        }
    }

    pub const fn key(self) -> Option<EventKey> {
        match self {
            EventPolicy::CoalesceLatest { key, .. } => Some(key),
            EventPolicy::DropIfStale { stale_key } => Some(stale_key),
            _ => None,
        }
    }
}

impl EventLane {
    pub const fn name(self) -> &'static str {
        match self {
            EventLane::Control => "control",
            EventLane::RemoteCommand => "remote_command",
            EventLane::WorkResult => "work_result",
            EventLane::Telemetry => "telemetry",
        }
    }
}

impl EventKey {
    pub const fn name(self) -> &'static str {
        match self {
            EventKey::AiThinking => "ai.thinking",
            EventKey::AiWork => "ai.work",
            EventKey::ArtResize => "art.resize",
            EventKey::ArtworkVideo => "artwork.video",
            EventKey::DownloadProgress => "download.progress",
            EventKey::DownloadResult => "download.result",
            EventKey::GuiSearchTicket => "gui.search.ticket",
            EventKey::LyricsVideo => "lyrics.video",
            EventKey::MediaArtVideo => "media_art.video",
            EventKey::PlayerAudioCodec => "player.audio_codec",
            EventKey::PlayerCacheTime => "player.cache_time",
            EventKey::PlayerDuration => "player.duration",
            EventKey::PlayerFileFormat => "player.file_format",
            EventKey::PlayerMetadata => "player.metadata",
            EventKey::PlayerPaused => "player.paused",
            EventKey::PlayerTimePos => "player.time_pos",
            EventKey::PlayerVolume => "player.volume",
            EventKey::PlaylistIntent => "playlist.intent",
            EventKey::ResolverVideo => "resolver.video",
            EventKey::SearchRequest => "search.request",
            EventKey::StreamingSeed => "streaming.seed",
            EventKey::ToolProgress => "tool.progress",
            EventKey::ToolResult => "tool.result",
            EventKey::TransferAuth => "transfer.auth",
            EventKey::TransferJob => "transfer.job",
            EventKey::UpdateCheck => "update.check",
            EventKey::VideoOverlayGeneration => "video.overlay.generation",
            EventKey::VideoOverlayPaused => "video.overlay.paused",
            EventKey::YtdlpHealVideo => "ytdlp.heal.video",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LatestInsert {
    pub should_wake: bool,
    pub replaced_existing: bool,
    pub evicted_oldest: bool,
}

/// A bounded latest-value store for telemetry-like owner events.
///
/// Producers insert by semantic key. The owner loop receives one wake event while
/// this buffer is non-empty; repeated updates replace the key in-place instead of
/// adding more work to the owner queue.
pub struct LatestEventBuffer<K, E> {
    events: HashMap<K, E>,
    order: VecDeque<K>,
    max_entries: usize,
    wake_pending: bool,
}

impl<K, E> LatestEventBuffer<K, E>
where
    K: Eq + Hash + Clone,
{
    pub fn new(max_entries: usize) -> Self {
        Self {
            events: HashMap::new(),
            order: VecDeque::new(),
            max_entries,
            wake_pending: false,
        }
    }

    pub fn insert(&mut self, key: K, event: E) -> LatestInsert {
        let replaced_existing = self.events.insert(key.clone(), event).is_some();
        let mut evicted_oldest = false;
        if replaced_existing {
            self.order.retain(|existing| existing != &key);
        } else if self.events.len() > self.max_entries
            && let Some(oldest) = self.order.pop_front()
        {
            self.events.remove(&oldest);
            evicted_oldest = true;
        }
        self.order.push_back(key);

        let should_wake = !self.wake_pending;
        self.wake_pending = true;
        LatestInsert {
            should_wake,
            replaced_existing,
            evicted_oldest,
        }
    }

    pub fn drain(&mut self) -> Vec<E> {
        self.wake_pending = false;
        let mut drained = Vec::with_capacity(self.events.len());
        while let Some(key) = self.order.pop_front() {
            if let Some(event) = self.events.remove(&key) {
                drained.push(event);
            }
        }
        drained
    }
}
