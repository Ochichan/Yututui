use std::hash::{Hash, Hasher};
use std::time::Instant;

use super::DaemonEngine;

impl DaemonEngine {
    /// Whether the scrobble observer needs a periodic playback heartbeat.
    pub fn media_scrobble_heartbeat_active(&self) -> bool {
        self.queue.current().is_some() && !self.playback.paused && self.loaded_video_id.is_some()
    }

    /// Latest normalized mpv clock without constructing the owned OS-media projection.
    pub fn media_position_update(&self) -> Option<(f64, Instant)> {
        Some((self.playback.time_pos?, self.playback.time_pos_at?))
    }

    /// Allocation-free fingerprint of every daemon field projected by [`Self::media_snapshot`],
    /// excluding ordinary position/time-anchor progress. The owner loop uses this to avoid
    /// rebuilding a tree of owned strings and paths after unrelated remote/API events.
    pub fn media_fingerprint(&self) -> u64 {
        let mut hash = std::collections::hash_map::DefaultHasher::new();
        let current = self.queue.current();
        current.map(|song| song.video_id.as_str()).hash(&mut hash);
        self.playback.paused.hash(&mut hash);
        self.loaded_video_id.is_some().hash(&mut hash);
        self.playback.volume.hash(&mut hash);
        self.playback.speed.to_bits().hash(&mut hash);
        self.playback.position_epoch.hash(&mut hash);
        self.playback.duration.map(f64::to_bits).hash(&mut hash);
        self.queue.shuffle.hash(&mut hash);
        match self.queue.repeat {
            crate::queue::Repeat::Off => 0u8,
            crate::queue::Repeat::All => 1,
            crate::queue::Repeat::One => 2,
        }
        .hash(&mut hash);
        self.queue.len().hash(&mut hash);
        self.queue.position().hash(&mut hash);
        self.queue
            .peek_next()
            .map(|song| song.video_id.as_str())
            .hash(&mut hash);
        self.media_can_seek().hash(&mut hash);

        if let Some(song) = current {
            song.title.hash(&mut hash);
            song.artist.hash(&mut hash);
            song.album.hash(&mut hash);
            song.duration.hash(&mut hash);
            song.is_radio_station().hash(&mut hash);
            song.youtube_id().hash(&mut hash);
            song.local_path.hash(&mut hash);
            self.library.is_favorite(&song.video_id).hash(&mut hash);
            self.signals.is_disliked(&song.video_id).hash(&mut hash);
            self.media_art
                .as_ref()
                .filter(|art| art.key == song.video_id)
                .map(|art| (&art.key, &art.path))
                .hash(&mut hash);
        }

        hash.finish()
    }
}
