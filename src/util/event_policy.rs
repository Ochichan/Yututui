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
pub enum LatestEventClass {
    /// Accepted one-shot/control state cannot be revoked by a different key.
    Protected,
    /// Request-scoped results may be obsolete and can yield to newer stale-able work.
    StaleResult,
    /// High-rate state is always replaceable under saturation.
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
    LocalScanProgress,
    MediaArtVideo,
    PlayerAudioCodec,
    PlayerCacheTime,
    PlayerDuration,
    PlayerFileFormat,
    PlayerMetadata,
    PlayerPaused,
    PlayerEof,
    PlayerError,
    PlayerTransportClosed,
    PlayerTimePos,
    PlayerVolume,
    PlaylistIntent,
    ResolverVideo,
    ScrobbleAuthDone,
    ScrobbleAuthFailed,
    ScrobbleAuthUrl,
    ScrobbleQueueDropped,
    ScrobbleQueueStalled,
    ScrobbleSessionInvalid,
    SearchRequest,
    Signal,
    StreamingSeed,
    ToolProgress,
    ToolResult,
    TransferAuth,
    TransferJob,
    UpdateCheck,
    VideoOverlayGeneration,
    VideoOverlayFailure,
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
            EventKey::LocalScanProgress => "local.scan.progress",
            EventKey::MediaArtVideo => "media_art.video",
            EventKey::PlayerAudioCodec => "player.audio_codec",
            EventKey::PlayerCacheTime => "player.cache_time",
            EventKey::PlayerDuration => "player.duration",
            EventKey::PlayerFileFormat => "player.file_format",
            EventKey::PlayerMetadata => "player.metadata",
            EventKey::PlayerPaused => "player.paused",
            EventKey::PlayerEof => "player.eof",
            EventKey::PlayerError => "player.error",
            EventKey::PlayerTransportClosed => "player.transport_closed",
            EventKey::PlayerTimePos => "player.time_pos",
            EventKey::PlayerVolume => "player.volume",
            EventKey::PlaylistIntent => "playlist.intent",
            EventKey::ResolverVideo => "resolver.video",
            EventKey::ScrobbleAuthDone => "scrobble.auth.done",
            EventKey::ScrobbleAuthFailed => "scrobble.auth.failed",
            EventKey::ScrobbleAuthUrl => "scrobble.auth.url",
            EventKey::ScrobbleQueueDropped => "scrobble.queue.dropped",
            EventKey::ScrobbleQueueStalled => "scrobble.queue.stalled",
            EventKey::ScrobbleSessionInvalid => "scrobble.session.invalid",
            EventKey::SearchRequest => "search.request",
            EventKey::Signal => "signal",
            EventKey::StreamingSeed => "streaming.seed",
            EventKey::ToolProgress => "tool.progress",
            EventKey::ToolResult => "tool.result",
            EventKey::TransferAuth => "transfer.auth",
            EventKey::TransferJob => "transfer.job",
            EventKey::UpdateCheck => "update.check",
            EventKey::VideoOverlayGeneration => "video.overlay.generation",
            EventKey::VideoOverlayFailure => "video.overlay.failure",
            EventKey::VideoOverlayPaused => "video.overlay.paused",
            EventKey::YtdlpHealVideo => "ytdlp.heal.video",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LatestInsert {
    pub accepted: bool,
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
    priorities: HashMap<K, LatestEventClass>,
    max_entries: usize,
    wake_pending: bool,
}

impl<K, E> LatestEventBuffer<K, E>
where
    K: Eq + Hash + Clone,
{
    pub fn new(max_entries: usize) -> Self {
        assert!(max_entries > 0, "latest-event capacity must be non-zero");
        Self {
            events: HashMap::new(),
            order: VecDeque::new(),
            priorities: HashMap::new(),
            max_entries,
            wake_pending: false,
        }
    }

    pub fn insert(&mut self, key: K, event: E) -> LatestInsert {
        self.insert_prioritized(key, event, LatestEventClass::Telemetry)
    }

    /// Insert a latest value according to its revocability. Telemetry and stale request results
    /// may be displaced after admission; accepted one-shot/control keys stay resident until the
    /// owner drains them or a newer value for the *same semantic key* replaces them. Protected
    /// values may displace revocable entries, while a full protected set rejects every new key.
    pub fn insert_prioritized(
        &mut self,
        key: K,
        event: E,
        priority: LatestEventClass,
    ) -> LatestInsert {
        let replaced_existing = self.events.contains_key(&key);
        let victim = if !replaced_existing && self.events.len() >= self.max_entries {
            let Some(victim) = self.order.iter().position(|candidate| {
                self.priorities
                    .get(candidate)
                    .is_some_and(|resident| match priority {
                        LatestEventClass::Telemetry => *resident == LatestEventClass::Telemetry,
                        LatestEventClass::StaleResult | LatestEventClass::Protected => {
                            *resident != LatestEventClass::Protected
                        }
                    })
            }) else {
                return LatestInsert {
                    accepted: false,
                    should_wake: false,
                    replaced_existing: false,
                    evicted_oldest: false,
                };
            };
            Some(victim)
        } else {
            None
        };

        if let Some(victim) = victim
            && let Some(evicted) = self.order.remove(victim)
        {
            self.events.remove(&evicted);
            self.priorities.remove(&evicted);
        }

        self.events.insert(key.clone(), event);
        self.priorities.insert(key.clone(), priority);
        let evicted_oldest = victim.is_some();
        if replaced_existing {
            self.order.retain(|existing| existing != &key);
        }
        self.order.push_back(key.clone());
        debug_assert!(self.events.len() <= self.max_entries);

        let should_wake = !self.wake_pending;
        self.wake_pending = true;
        LatestInsert {
            accepted: true,
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
                self.priorities.remove(&key);
                drained.push(event);
            }
        }
        drained
    }

    /// Re-arm wake delivery after the owner wake could not be accepted.
    ///
    /// Callers must hold the buffer lock from insertion through the failed wake attempt so
    /// another producer cannot observe a stale `wake_pending` value in between.
    pub fn rearm_wake(&mut self) {
        self.wake_pending = false;
    }

    /// Drop buffered values when their owner queue has permanently closed.
    pub fn clear(&mut self) {
        self.events.clear();
        self.order.clear();
        self.priorities.clear();
        self.wake_pending = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn latest_buffer_refreshes_replaced_keys_and_evicts_the_oldest_key() {
        let mut buffer = LatestEventBuffer::new(2);
        assert!(buffer.insert("a", 1).should_wake);
        assert!(!buffer.insert("b", 2).should_wake);

        let replaced = buffer.insert("a", 3);
        assert!(replaced.replaced_existing);
        assert!(!replaced.evicted_oldest);

        let evicted = buffer.insert("c", 4);
        assert!(!evicted.replaced_existing);
        assert!(evicted.evicted_oldest);
        assert_eq!(buffer.drain(), vec![3, 4]);
    }

    #[test]
    fn protected_control_key_evicts_ordinary_work_instead() {
        let mut buffer = LatestEventBuffer::new(2);
        assert!(
            buffer
                .insert_prioritized("control", 1, LatestEventClass::Protected)
                .should_wake
        );
        buffer.insert("old-work", 2);
        let inserted = buffer.insert("new-work", 3);

        assert!(inserted.evicted_oldest);
        assert_eq!(buffer.drain(), vec![1, 3]);
    }

    #[test]
    fn incoming_unprotected_value_is_rejected_when_every_resident_is_protected() {
        let mut buffer = LatestEventBuffer::new(1);
        assert!(
            buffer
                .insert_prioritized("control", 1, LatestEventClass::Protected)
                .should_wake
        );

        let inserted = buffer.insert("result", 2);

        assert!(!inserted.accepted);
        assert!(!inserted.evicted_oldest);
        assert_eq!(buffer.drain(), vec![1]);
    }

    #[test]
    fn a_new_protected_key_cannot_revoke_an_accepted_protected_key() {
        let mut buffer = LatestEventBuffer::new(1);
        assert!(
            buffer
                .insert_prioritized("work", 1, LatestEventClass::Protected)
                .accepted
        );

        let control = buffer.insert_prioritized("control", 2, LatestEventClass::Protected);

        assert!(!control.accepted);
        assert_eq!(buffer.drain(), vec![1]);
    }

    #[test]
    fn newer_stale_result_displaces_the_oldest_stale_result() {
        let mut buffer = LatestEventBuffer::new(2);
        buffer.insert_prioritized("request-1", 1, LatestEventClass::StaleResult);
        buffer.insert_prioritized("request-2", 2, LatestEventClass::StaleResult);

        let newest = buffer.insert_prioritized("request-3", 3, LatestEventClass::StaleResult);

        assert!(newest.accepted);
        assert!(newest.evicted_oldest);
        assert_eq!(buffer.drain(), vec![2, 3]);
    }

    #[test]
    fn telemetry_cannot_displace_a_stale_result_or_protected_value() {
        let mut buffer = LatestEventBuffer::new(2);
        buffer.insert_prioritized("request", 1, LatestEventClass::StaleResult);
        buffer.insert_prioritized("control", 2, LatestEventClass::Protected);

        let telemetry = buffer.insert("progress", 3);

        assert!(!telemetry.accepted);
        assert_eq!(buffer.drain(), vec![1, 2]);
    }

    #[test]
    fn protected_value_can_displace_a_stale_result() {
        let mut buffer = LatestEventBuffer::new(1);
        buffer.insert_prioritized("request", 1, LatestEventClass::StaleResult);

        let control = buffer.insert_prioritized("control", 2, LatestEventClass::Protected);

        assert!(control.accepted);
        assert!(control.evicted_oldest);
        assert_eq!(buffer.drain(), vec![2]);
    }
}
