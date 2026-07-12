//! Daemon-side host for the lyrics actor (B1 `lyrics` topic).
//!
//! The TUI owner drives `crate::lyrics` from its own panel reducer; the daemon has no
//! panel, so this host watches the current track on the owner lane and keeps the
//! session `lyrics` topic current instead. Fetches are gated on a live subscriber —
//! a headless daemon with no GUI attached never talks to lrclib.

use crate::api::Song;
use crate::lyrics::{LyricLine, LyricsHandle};
use crate::remote::proto::LyricLineModel;
use crate::remote::publish::Publisher;

use super::events::{DaemonEvent, DaemonEventSender, record_daemon_event};

pub(super) struct LyricsHost {
    handle: LyricsHandle,
    /// The track whose lyrics were last requested/published (`None` = nothing playing
    /// or no subscriber yet).
    current: Option<String>,
}

impl LyricsHost {
    /// Spawn the (cached, latest-only) lyrics actor; results come back on the owner
    /// lane as [`DaemonEvent::Lyrics`].
    pub(super) fn spawn(event_tx: DaemonEventSender) -> Self {
        let handle = crate::lyrics::spawn(move |event| {
            record_daemon_event(&event_tx, DaemonEvent::Lyrics(event));
        });
        Self {
            handle,
            current: None,
        }
    }

    /// Per-turn observer, called next to `Publisher::observe`: when a lyrics subscriber
    /// exists and the current track changed, clear the topic immediately (so a client
    /// never highlights the previous track's lines) and start the fetch.
    pub(super) fn observe(&mut self, publisher: &mut Publisher, current: Option<&Song>) {
        if !publisher.lyrics_subscribed() {
            return;
        }
        let current_id = current.map(|song| song.video_id.as_str());
        if self.current.as_deref() == current_id {
            return;
        }
        self.current = current_id.map(str::to_owned);
        publisher.publish_lyrics(self.current.clone(), Vec::new());
        if let Some(song) = current {
            // A delivery failure only means the actor is gone (process teardown); the
            // cleared snapshot above is already the correct terminal state.
            let _ = self.handle.fetch(
                song.video_id.clone(),
                song.artist.clone(),
                song.title.clone(),
            );
        }
    }

    /// A fetch resolved. Publish only when it still names the current track — a rapid
    /// skip's stale result must never overwrite the newer clearing push.
    pub(super) fn on_result(
        &self,
        publisher: &mut Publisher,
        video_id: String,
        lines: &[LyricLine],
    ) {
        if self.current.as_deref() != Some(video_id.as_str()) {
            return;
        }
        publisher.publish_lyrics(Some(video_id), to_models(lines));
    }
}

/// Seconds → whole milliseconds, non-negative. All lrclib lines are synced, so `ms` is
/// always `Some` here; the wire keeps `Option` for unsynced sources (docs/gui/02 §7).
fn to_models(lines: &[LyricLine]) -> Vec<LyricLineModel> {
    lines
        .iter()
        .map(|line| LyricLineModel {
            ms: Some((line.time.max(0.0) * 1000.0).round() as u64),
            text: line.text.clone(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_seconds_to_clamped_millis() {
        let lines = [
            LyricLine {
                time: 12.5,
                text: "twelve and a half".to_owned(),
            },
            LyricLine {
                time: -3.0,
                text: "clamped".to_owned(),
            },
        ];
        let models = to_models(&lines);
        assert_eq!(models[0].ms, Some(12_500));
        assert_eq!(models[0].text, "twelve and a half");
        assert_eq!(models[1].ms, Some(0));
    }
}
