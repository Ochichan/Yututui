//! Linux MPRIS adapter (`org.mpris.MediaPlayer2.ytmtui` on the session bus), per
//! the media-controls spec §3, built on `mpris-server` (zbus).
//!
//! The D-Bus server runs on its own tokio task: the facade forwards diffed
//! snapshots over a channel; the task keeps the latest snapshot in shared state
//! (so property *reads* — including the un-signalled, interpolated `Position` —
//! answer instantly), batches changed properties into a single
//! `PropertiesChanged` per logical event (L-6/S-4), and emits `Seeked` exactly on
//! position discontinuities (L-7). Inbound calls only forward a [`MediaCommand`]
//! and return (C-1).
//!
//! No session bus (SSH/headless/container) is non-fatal: the facade logs one
//! warning and the app runs without media controls (spec A-2).

use std::sync::{Arc, Mutex};

use anyhow::Result;
use mpris_server::{
    LoopStatus, Metadata, PlaybackRate, PlaybackStatus, Property, Server, Signal, Time, TrackId,
    Volume,
    zbus::{self, fdo},
};
use tokio::sync::mpsc;

use super::{
    CommandSink, LatestMediaUpdate, MediaChanges, MediaCommand, MediaPlaybackStatus, MediaSnapshot,
};
use crate::config::{SPEED_MAX, SPEED_MIN};
use crate::queue::Repeat;
use crate::util::delivery::{DeliveryError, DeliveryReceipt, DeliveryResult};

/// MPRIS players register at launch: Linux widgets list every player, there is no
/// single ownership slot to steal, and an eagerly listed (paused) player lets the
/// desktop offer "resume last session" right from the widget.
pub const EAGER: bool = true;

/// Bus-name suffix: `org.mpris.MediaPlayer2.ytmtui`.
const BUS_SUFFIX: &str = "ytmtui";

pub struct Backend {
    wake_tx: mpsc::Sender<()>,
    pending: Arc<LatestMediaUpdate>,
}

impl Backend {
    pub fn new(sink: CommandSink) -> Result<Self> {
        // One wake plus one latest snapshot bounds memory while retaining every diff
        // facet through `MediaChanges::merge` during a slow D-Bus call.
        let (wake_tx, wake_rx) = mpsc::channel(1);
        let pending = Arc::new(LatestMediaUpdate::default());
        tokio::spawn(run_server(wake_rx, Arc::clone(&pending), sink));
        Ok(Self { wake_tx, pending })
    }

    pub fn apply(&mut self, snapshot: &MediaSnapshot, changes: MediaChanges) -> DeliveryResult {
        if self.wake_tx.is_closed() {
            return Err(DeliveryError::Closed);
        }
        let replaced_existing = self.pending.store(snapshot.clone(), changes);
        match self.wake_tx.try_send(()) {
            Ok(()) if replaced_existing => Ok(DeliveryReceipt::Coalesced {
                replaced_existing: true,
                evicted_oldest: false,
            }),
            Ok(()) => Ok(DeliveryReceipt::Enqueued),
            Err(mpsc::error::TrySendError::Full(())) => Ok(DeliveryReceipt::Coalesced {
                replaced_existing: true,
                evicted_oldest: false,
            }),
            Err(mpsc::error::TrySendError::Closed(())) => {
                self.pending.clear();
                Err(DeliveryError::Closed)
            }
        }
    }
}

async fn run_server(
    mut wake_rx: mpsc::Receiver<()>,
    pending: Arc<LatestMediaUpdate>,
    sink: CommandSink,
) {
    let state = Arc::new(Mutex::new(MediaSnapshot::idle()));
    // Bus-name collision (a second instance) retries with a PID-qualified suffix,
    // as the MPRIS spec prescribes (spec L-2).
    let server = match Server::new(
        BUS_SUFFIX,
        Player {
            sink: Arc::clone(&sink_holder(&sink)),
            state: Arc::clone(&state),
        },
    )
    .await
    {
        Ok(server) => server,
        Err(first_err) => {
            let suffix = format!("{BUS_SUFFIX}.instance{}", std::process::id());
            match Server::new(
                &suffix,
                Player {
                    sink: Arc::clone(&sink_holder(&sink)),
                    state: Arc::clone(&state),
                },
            )
            .await
            {
                Ok(server) => server,
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        first_error = %first_err,
                        "media controls disabled: could not register MPRIS service"
                    );
                    return;
                }
            }
        }
    };
    tracing::info!(bus_name = %server.bus_name(), "media controls: MPRIS session ready");

    while wake_rx.recv().await.is_some() {
        let Some((snapshot, changes)) = pending.take() else {
            continue;
        };
        *state.lock().unwrap() = snapshot.clone();
        let mut properties = Vec::new();
        if changes.track || changes.artwork {
            properties.push(Property::Metadata(build_metadata(&snapshot)));
        }
        if changes.status {
            properties.push(Property::PlaybackStatus(playback_status(&snapshot)));
        }
        if changes.options {
            properties.push(Property::Rate(snapshot.rate));
            properties.push(Property::Volume(snapshot.volume));
            properties.push(Property::Shuffle(snapshot.shuffle));
            properties.push(Property::LoopStatus(loop_status(snapshot.repeat)));
        }
        if changes.caps {
            properties.push(Property::CanGoNext(snapshot.caps.can_next));
            properties.push(Property::CanGoPrevious(snapshot.caps.can_previous));
            properties.push(Property::CanPlay(snapshot.caps.can_play));
            properties.push(Property::CanPause(snapshot.caps.can_pause));
            properties.push(Property::CanSeek(snapshot.caps.can_seek));
        }
        // One PropertiesChanged per logical event (L-6/S-4).
        if !properties.is_empty()
            && let Err(e) = server.properties_changed(properties).await
        {
            tracing::debug!(error = %e, "MPRIS properties_changed failed");
        }
        // Seeked fires on every discontinuity — user seek, SetPosition, and track
        // (re)starts as Seeked(0) (L-7); never on plain progress (L-8).
        if changes.position
            && let Err(e) = server
                .emit(Signal::Seeked {
                    position: Time::from_micros((snapshot.position_now() * 1e6) as i64),
                })
                .await
        {
            tracing::debug!(error = %e, "MPRIS Seeked emit failed");
        }
    }
    // Facade dropped the sender (disable/quit): release the bus name explicitly so
    // desktop widgets drop the entry immediately (L-3).
    let _ = server.release_bus_name().await;
}

/// `Arc`-clone helper so each `Server::new` attempt gets its own `Player`.
fn sink_holder(sink: &CommandSink) -> CommandSink {
    Arc::clone(sink)
}

struct Player {
    sink: CommandSink,
    state: Arc<Mutex<MediaSnapshot>>,
}

impl Player {
    fn snapshot(&self) -> MediaSnapshot {
        self.state.lock().unwrap().clone()
    }

    fn send_fdo(&self, cmd: MediaCommand) -> fdo::Result<()> {
        (self.sink)(cmd)
            .map(|_| ())
            .map_err(|error| fdo::Error::Failed(format!("owner rejected media command: {error}")))
    }

    fn send_zbus(&self, cmd: MediaCommand) -> zbus::Result<()> {
        (self.sink)(cmd)
            .map(|_| ())
            .map_err(|error| zbus::Error::Failure(format!("owner rejected media command: {error}")))
    }
}

fn playback_status(snapshot: &MediaSnapshot) -> PlaybackStatus {
    match snapshot.status {
        MediaPlaybackStatus::Playing => PlaybackStatus::Playing,
        MediaPlaybackStatus::Paused => PlaybackStatus::Paused,
        MediaPlaybackStatus::Stopped => PlaybackStatus::Stopped,
    }
}

fn loop_status(repeat: Repeat) -> LoopStatus {
    match repeat {
        Repeat::Off => LoopStatus::None,
        Repeat::One => LoopStatus::Track,
        Repeat::All => LoopStatus::Playlist,
    }
}

/// The `mpris:trackid` object path for a queue track key (spec §6.1): YouTube ids
/// use base64url (`A-Z a-z 0-9 _ -`) but D-Bus object paths only allow
/// `[A-Za-z0-9_]`, so every byte outside `[A-Za-z0-9]` is escaped as `_` + 2 hex
/// digits — reversible, so `SetPosition`'s stale-track guard can compare ids.
pub(crate) fn track_id_path(key: &str) -> String {
    let mut escaped = String::with_capacity(key.len());
    for byte in key.bytes() {
        if byte.is_ascii_alphanumeric() {
            escaped.push(byte as char);
        } else {
            escaped.push('_');
            escaped.push_str(&format!("{byte:02X}"));
        }
    }
    if escaped.is_empty() {
        escaped.push_str("unknown");
    }
    format!("/org/mpris/MediaPlayer2/ytmtui/track/{escaped}")
}

/// Reverse of [`track_id_path`], for tests and debugging.
#[cfg(test)]
pub(crate) fn track_key_from_path(path: &str) -> Option<String> {
    let escaped = path.strip_prefix("/org/mpris/MediaPlayer2/ytmtui/track/")?;
    let mut out = Vec::new();
    let mut bytes = escaped.bytes();
    while let Some(b) = bytes.next() {
        if b == b'_' {
            let hi = bytes.next()?;
            let lo = bytes.next()?;
            let hex = [hi, lo];
            let hex = std::str::from_utf8(&hex).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
        } else {
            out.push(b);
        }
    }
    String::from_utf8(out).ok()
}

fn build_metadata(snapshot: &MediaSnapshot) -> Metadata {
    // No track → empty map (spec L-4); never a fake NoTrack path for real tracks.
    let Some(track) = &snapshot.track else {
        return Metadata::new();
    };
    let mut builder = Metadata::builder().title(track.title.clone());
    if let Ok(trackid) = TrackId::try_from(track_id_path(&track.key)) {
        builder = builder.trackid(trackid);
    }
    if !track.artist.is_empty() {
        // xesam:artist is an array; the app's display artist stays one element
        // (splitting on commas would mangle names that contain them).
        builder = builder.artist([track.artist.clone()]);
    }
    if let Some(album) = track.album.clone().filter(|a| !a.is_empty()) {
        builder = builder.album(album);
    }
    // Live streams omit mpris:length entirely (spec table §2.6).
    if let Some(duration) = track.duration.filter(|_| !track.is_live) {
        builder = builder.length(Time::from_micros((duration * 1e6) as i64));
    }
    // Prefer the cached file:// URI; fall back to the remote https URL until the
    // cache lands (most modern MPRIS clients accept both).
    if let Some(path) = &track.art_file {
        builder = builder.art_url(format!("file://{}", path.display()));
    } else if let Some(url) = &track.art_remote_url {
        builder = builder.art_url(url.clone());
    }
    if let Some(url) = &track.url {
        builder = builder.url(url.clone());
    }
    builder.build()
}

impl mpris_server::RootInterface for Player {
    async fn raise(&self) -> fdo::Result<()> {
        // CanRaise is false — a terminal app has no reliable way to raise itself.
        Ok(())
    }

    async fn quit(&self) -> fdo::Result<()> {
        self.send_fdo(MediaCommand::Quit)
    }

    async fn can_quit(&self) -> fdo::Result<bool> {
        Ok(true)
    }

    async fn fullscreen(&self) -> fdo::Result<bool> {
        Ok(false)
    }

    async fn set_fullscreen(&self, _fullscreen: bool) -> zbus::Result<()> {
        Ok(())
    }

    async fn can_set_fullscreen(&self) -> fdo::Result<bool> {
        Ok(false)
    }

    async fn can_raise(&self) -> fdo::Result<bool> {
        Ok(false)
    }

    async fn has_track_list(&self) -> fdo::Result<bool> {
        Ok(false)
    }

    async fn identity(&self) -> fdo::Result<String> {
        Ok("YuTuTui!".to_owned())
    }

    async fn desktop_entry(&self) -> fdo::Result<String> {
        Ok("yututui".to_owned())
    }

    async fn supported_uri_schemes(&self) -> fdo::Result<Vec<String>> {
        Ok(vec!["https".to_owned()])
    }

    async fn supported_mime_types(&self) -> fdo::Result<Vec<String>> {
        Ok(Vec::new())
    }
}

impl mpris_server::PlayerInterface for Player {
    async fn next(&self) -> fdo::Result<()> {
        self.send_fdo(MediaCommand::Next)
    }

    async fn previous(&self) -> fdo::Result<()> {
        self.send_fdo(MediaCommand::Previous)
    }

    async fn pause(&self) -> fdo::Result<()> {
        self.send_fdo(MediaCommand::Pause)
    }

    async fn play_pause(&self) -> fdo::Result<()> {
        self.send_fdo(MediaCommand::Toggle)
    }

    async fn stop(&self) -> fdo::Result<()> {
        self.send_fdo(MediaCommand::Stop)
    }

    async fn play(&self) -> fdo::Result<()> {
        self.send_fdo(MediaCommand::Play)
    }

    async fn seek(&self, offset: Time) -> fdo::Result<()> {
        self.send_fdo(MediaCommand::SeekBy(offset.as_micros() as f64 / 1e6))
    }

    async fn set_position(&self, track_id: TrackId, position: Time) -> fdo::Result<()> {
        // Stale-call guard (spec §3.5): a SetPosition raced against a track change
        // must be dropped, not applied to the new track.
        let current = self.snapshot().track.map(|track| track_id_path(&track.key));
        if current.as_deref() == Some(track_id.as_str()) {
            return self.send_fdo(MediaCommand::SeekTo(position.as_micros() as f64 / 1e6));
        }
        Ok(())
    }

    async fn open_uri(&self, uri: String) -> fdo::Result<()> {
        self.send_fdo(MediaCommand::OpenUri(uri))
    }

    async fn playback_status(&self) -> fdo::Result<PlaybackStatus> {
        Ok(playback_status(&self.snapshot()))
    }

    async fn loop_status(&self) -> fdo::Result<LoopStatus> {
        Ok(loop_status(self.snapshot().repeat))
    }

    async fn set_loop_status(&self, loop_status: LoopStatus) -> zbus::Result<()> {
        self.send_zbus(MediaCommand::SetRepeat(match loop_status {
            LoopStatus::None => Repeat::Off,
            LoopStatus::Track => Repeat::One,
            LoopStatus::Playlist => Repeat::All,
        }))
    }

    async fn rate(&self) -> fdo::Result<PlaybackRate> {
        Ok(self.snapshot().rate)
    }

    async fn set_rate(&self, rate: PlaybackRate) -> zbus::Result<()> {
        // 0.0 pauses per the MPRIS spec; the core clamps into [MinimumRate,
        // MaximumRate] and ignores unusable values.
        self.send_zbus(MediaCommand::SetRate(rate))
    }

    async fn shuffle(&self) -> fdo::Result<bool> {
        Ok(self.snapshot().shuffle)
    }

    async fn set_shuffle(&self, shuffle: bool) -> zbus::Result<()> {
        self.send_zbus(MediaCommand::SetShuffle(shuffle))
    }

    async fn metadata(&self) -> fdo::Result<Metadata> {
        Ok(build_metadata(&self.snapshot()))
    }

    async fn volume(&self) -> fdo::Result<Volume> {
        Ok(self.snapshot().volume)
    }

    async fn set_volume(&self, volume: Volume) -> zbus::Result<()> {
        // Negative writes clamp to 0.0 (spec requirement); the core clamps.
        self.send_zbus(MediaCommand::SetVolume(volume))
    }

    async fn position(&self) -> fdo::Result<Time> {
        // Served from the interpolated position clock at read time; Position never
        // appears in PropertiesChanged (L-8) — clients interpolate via Rate.
        Ok(Time::from_micros(
            (self.snapshot().position_now() * 1e6) as i64,
        ))
    }

    async fn minimum_rate(&self) -> fdo::Result<PlaybackRate> {
        Ok(SPEED_MIN)
    }

    async fn maximum_rate(&self) -> fdo::Result<PlaybackRate> {
        Ok(SPEED_MAX)
    }

    async fn can_go_next(&self) -> fdo::Result<bool> {
        Ok(self.snapshot().caps.can_next)
    }

    async fn can_go_previous(&self) -> fdo::Result<bool> {
        Ok(self.snapshot().caps.can_previous)
    }

    async fn can_play(&self) -> fdo::Result<bool> {
        Ok(self.snapshot().caps.can_play)
    }

    async fn can_pause(&self) -> fdo::Result<bool> {
        Ok(self.snapshot().caps.can_pause)
    }

    async fn can_seek(&self) -> fdo::Result<bool> {
        Ok(self.snapshot().caps.can_seek)
    }

    async fn can_control(&self) -> fdo::Result<bool> {
        // Constant true; CanControl has no change signal, so it must never move.
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn track_id_escapes_per_spec() {
        // Plain alphanumerics pass through; `-`(0x2D) and `_`(0x5F) escape.
        assert_eq!(
            track_id_path("dQw4w9WgXcQ"),
            "/org/mpris/MediaPlayer2/ytmtui/track/dQw4w9WgXcQ"
        );
        assert_eq!(
            track_id_path("a-b_c"),
            "/org/mpris/MediaPlayer2/ytmtui/track/a_2Db_5Fc"
        );
    }

    #[test]
    fn track_id_round_trips() {
        for key in ["dQw4w9WgXcQ", "a-b_c", "local:/tmp/음악.m4a", ""] {
            let path = track_id_path(key);
            // The escaped form is a valid D-Bus object path.
            assert!(TrackId::try_from(path.clone()).is_ok(), "{path}");
            if !key.is_empty() {
                assert_eq!(track_key_from_path(&path).as_deref(), Some(key));
            }
        }
    }

    #[test]
    fn command_rejection_is_returned_to_dbus_callers() {
        let player = Player {
            sink: Arc::new(|_| Err(DeliveryError::Busy)),
            state: Arc::new(Mutex::new(MediaSnapshot::idle())),
        };

        assert!(matches!(
            player.send_fdo(MediaCommand::Play),
            Err(fdo::Error::Failed(message)) if message.contains("busy")
        ));
        assert!(matches!(
            player.send_zbus(MediaCommand::Pause),
            Err(zbus::Error::Failure(message)) if message.contains("busy")
        ));
    }
}
