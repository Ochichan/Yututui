//! macOS Now Playing adapter: `MPNowPlayingInfoCenter` (state out) +
//! `MPRemoteCommandCenter` (commands in), per the media-controls spec §5.
//!
//! macOS specifics honored here:
//! - `playbackState` is set explicitly and always together with
//!   `MPNowPlayingInfoPropertyPlaybackRate` (spec M-2) — macOS cannot infer playback
//!   state from an audio session, and mismatched state/rate desyncs the Control
//!   Center button and progress bar.
//! - `ElapsedPlaybackTime` is only written on discontinuities (track change, seek,
//!   pause/resume, rate change) — never periodically (spec M-3); the OS interpolates.
//! - Only commands we actually support are registered; `skipForward`/`skipBackward`
//!   stay disabled so Control Center keeps real Next/Previous buttons (spec §5.4).
//! - Remote-command handlers land on the main dispatch queue, which a TUI never
//!   drains by itself — the run loop calls [`Backend::pump`] on a short interval to
//!   service it (spec M-6). Handlers only forward a [`MediaCommand`] and return.
//!
//! The backend lives on the main thread (the run loop's `block_on` future) and is
//! deliberately not `Send`.

use std::path::PathBuf;
use std::ptr::NonNull;

use anyhow::Result;
use block2::RcBlock;
use objc2::AnyThread;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2_app_kit::NSImage;
use objc2_core_foundation::{CFRunLoop, CGSize, kCFRunLoopDefaultMode};
use objc2_foundation::{NSMutableDictionary, NSNumber, NSString};
use objc2_media_player::{
    MPChangePlaybackPositionCommandEvent, MPChangeRepeatModeCommandEvent,
    MPChangeShuffleModeCommandEvent, MPMediaItemArtwork, MPMediaItemPropertyArtist,
    MPMediaItemPropertyArtwork, MPMediaItemPropertyPlaybackDuration, MPMediaItemPropertyTitle,
    MPNowPlayingInfoCenter, MPNowPlayingInfoMediaType, MPNowPlayingInfoPropertyDefaultPlaybackRate,
    MPNowPlayingInfoPropertyElapsedPlaybackTime, MPNowPlayingInfoPropertyIsLiveStream,
    MPNowPlayingInfoPropertyMediaType, MPNowPlayingInfoPropertyPlaybackRate,
    MPNowPlayingPlaybackState, MPRemoteCommand, MPRemoteCommandCenter, MPRemoteCommandEvent,
    MPRemoteCommandHandlerStatus, MPRepeatType, MPShuffleType,
};

use super::{CommandSink, MediaChanges, MediaCommand, MediaPlaybackStatus, MediaSnapshot};
use crate::queue::Repeat;
use crate::t;

/// Claim the Now Playing slot lazily (first actual playback), never at launch —
/// macOS has a single system-wide slot and merely opening the app must not steal it
/// from whatever the user was listening to (spec M-7).
pub const EAGER: bool = false;

pub struct Backend {
    center: Retained<MPNowPlayingInfoCenter>,
    commands: Retained<MPRemoteCommandCenter>,
    /// The artwork object currently referenced from `nowPlayingInfo`, keyed by the
    /// cache file it was loaded from so re-applies don't re-decode the image.
    artwork: Option<(PathBuf, Retained<MPMediaItemArtwork>)>,
}

impl Backend {
    pub fn new(sink: CommandSink) -> Result<Self> {
        // SAFETY: objc2 exposes these MediaPlayer singleton accessors as unsafe; they
        // return retained Objective-C objects or abort through the binding on contract failure.
        let center = unsafe { MPNowPlayingInfoCenter::defaultCenter() };
        // SAFETY: same singleton contract as above for the remote command center.
        let commands = unsafe { MPRemoteCommandCenter::sharedCommandCenter() };
        let backend = Self {
            center,
            commands,
            artwork: None,
        };
        backend.register_commands(&sink);
        tracing::info!("media controls: macOS Now Playing session ready");
        Ok(backend)
    }

    /// Drain the main run loop / main dispatch queue once, non-blocking. Remote
    /// command handlers registered below are delivered during this call.
    pub fn pump(&mut self) {
        // SAFETY: called on the backend's main-thread owner; mode is the constant
        // default run-loop mode, duration is zero, so this only pumps pending work.
        unsafe {
            CFRunLoop::run_in_mode(kCFRunLoopDefaultMode, 0.0, false);
        }
    }

    fn register_commands(&self, sink: &CommandSink) {
        let c = &self.commands;
        // SAFETY: all commands come from the live MPRemoteCommandCenter singleton and
        // handler blocks only enqueue app commands before returning to MediaPlayer.
        unsafe {
            register(&c.playCommand(), sink, |_| Some(MediaCommand::Play));
            register(&c.pauseCommand(), sink, |_| Some(MediaCommand::Pause));
            // AirPods stem clicks / headphone buttons arrive as togglePlayPause —
            // it must be registered separately from play/pause.
            register(&c.togglePlayPauseCommand(), sink, |_| {
                Some(MediaCommand::Toggle)
            });
            register(&c.stopCommand(), sink, |_| Some(MediaCommand::Stop));
            register(&c.nextTrackCommand(), sink, |_| Some(MediaCommand::Next));
            register(&c.previousTrackCommand(), sink, |_| {
                Some(MediaCommand::Previous)
            });
            // Registering this is what makes Control Center show a scrubber.
            register(&c.changePlaybackPositionCommand(), sink, |event| {
                let event = event.downcast_ref::<MPChangePlaybackPositionCommandEvent>()?;
                Some(MediaCommand::SeekTo(event.positionTime()))
            });
            register(&c.changeShuffleModeCommand(), sink, |event| {
                let event = event.downcast_ref::<MPChangeShuffleModeCommandEvent>()?;
                Some(MediaCommand::SetShuffle(
                    event.shuffleType() != MPShuffleType::Off,
                ))
            });
            register(&c.changeRepeatModeCommand(), sink, |event| {
                let event = event.downcast_ref::<MPChangeRepeatModeCommandEvent>()?;
                Some(MediaCommand::SetRepeat(match event.repeatType() {
                    MPRepeatType::One => Repeat::One,
                    MPRepeatType::All => Repeat::All,
                    _ => Repeat::Off,
                }))
            });
            // Like/dislike mirror the TUI's `f` rating cycle onto the OS surface.
            let like = c.likeCommand();
            like.setLocalizedTitle(&NSString::from_str(t!("Like", "좋아요")));
            register(&like, sink, |_| Some(MediaCommand::Like));
            let dislike = c.dislikeCommand();
            dislike.setLocalizedTitle(&NSString::from_str(t!("Dislike", "싫어요")));
            register(&dislike, sink, |_| Some(MediaCommand::Dislike));

            // Explicitly unsupported (spec M-5): skip/seek-interval commands would
            // replace Next/Previous in Control Center; rate/bookmark aren't offered.
            c.skipForwardCommand().setEnabled(false);
            c.skipBackwardCommand().setEnabled(false);
            c.seekForwardCommand().setEnabled(false);
            c.seekBackwardCommand().setEnabled(false);
            c.changePlaybackRateCommand().setEnabled(false);
            c.ratingCommand().setEnabled(false);
            c.bookmarkCommand().setEnabled(false);
        }
    }

    pub fn apply(&mut self, snapshot: &MediaSnapshot, changes: MediaChanges) {
        // SAFETY: the backend owns all retained MediaPlayer/Foundation objects on the
        // main thread and writes only Foundation object values matching Apple's keys.
        unsafe {
            match &snapshot.track {
                None => {
                    if changes.track {
                        self.artwork = None;
                        self.center.setNowPlayingInfo(None);
                        self.center
                            .setPlaybackState(MPNowPlayingPlaybackState::Stopped);
                    }
                }
                Some(_) => {
                    if changes.track {
                        // Track change: replace the dictionary wholesale so no key
                        // from the previous track lingers (spec M-1).
                        let info = self.build_info(snapshot);
                        self.center.setNowPlayingInfo(Some(&info));
                    } else if changes.artwork
                        || changes.position
                        || changes.status
                        || changes.options
                    {
                        // Partial change: copy-modify-reassign the existing info.
                        // Elapsed/rate are rewritten only on real discontinuities
                        // (seek, pause/resume, rate change) per spec M-3.
                        let info = match self.center.nowPlayingInfo() {
                            Some(current) => {
                                objc2_foundation::NSMutableCopying::mutableCopy(&*current)
                            }
                            None => self.build_info(snapshot),
                        };
                        if changes.artwork {
                            self.set_artwork(&info, snapshot);
                        }
                        if changes.position || changes.status || changes.options {
                            set_number(
                                &info,
                                MPNowPlayingInfoPropertyElapsedPlaybackTime,
                                snapshot.position_now(),
                            );
                            set_number(
                                &info,
                                MPNowPlayingInfoPropertyPlaybackRate,
                                effective_rate(snapshot),
                            );
                            set_number(
                                &info,
                                MPNowPlayingInfoPropertyDefaultPlaybackRate,
                                snapshot.rate,
                            );
                        }
                        self.center.setNowPlayingInfo(Some(&info));
                    }
                    if changes.track || changes.status {
                        // Spec M-2: playbackState and PlaybackRate move as a pair —
                        // the rate was written above whenever status changed.
                        self.center.setPlaybackState(match snapshot.status {
                            MediaPlaybackStatus::Playing => MPNowPlayingPlaybackState::Playing,
                            MediaPlaybackStatus::Paused => MPNowPlayingPlaybackState::Paused,
                            MediaPlaybackStatus::Stopped => MPNowPlayingPlaybackState::Stopped,
                        });
                    }
                }
            }

            if changes.caps || changes.track {
                let caps = snapshot.caps;
                let c = &self.commands;
                c.playCommand().setEnabled(caps.can_play);
                c.pauseCommand().setEnabled(caps.can_pause);
                c.togglePlayPauseCommand()
                    .setEnabled(caps.can_play || caps.can_pause);
                c.stopCommand().setEnabled(caps.can_pause);
                c.nextTrackCommand().setEnabled(caps.can_next);
                c.previousTrackCommand().setEnabled(caps.can_previous);
                c.changePlaybackPositionCommand().setEnabled(caps.can_seek);
                let has_track = snapshot.track.is_some();
                let ratable = has_track && !snapshot.track.as_ref().is_some_and(|t| t.is_live);
                c.likeCommand().setEnabled(has_track);
                c.dislikeCommand().setEnabled(ratable);
                c.changeShuffleModeCommand().setEnabled(true);
                c.changeRepeatModeCommand().setEnabled(true);
            }
            if changes.options || changes.track {
                // Reflect shuffle/repeat back so the Control Center toggles match
                // the app (the macOS analog of SMTC's W-2 requirement).
                self.commands
                    .changeShuffleModeCommand()
                    .setCurrentShuffleType(if snapshot.shuffle {
                        MPShuffleType::Items
                    } else {
                        MPShuffleType::Off
                    });
                self.commands
                    .changeRepeatModeCommand()
                    .setCurrentRepeatType(match snapshot.repeat {
                        Repeat::Off => MPRepeatType::Off,
                        Repeat::One => MPRepeatType::One,
                        Repeat::All => MPRepeatType::All,
                    });
            }
            if changes.feedback || changes.track {
                let (liked, disliked) = snapshot
                    .track
                    .as_ref()
                    .map(|t| (t.liked, t.disliked))
                    .unwrap_or((false, false));
                self.commands.likeCommand().setActive(liked);
                self.commands.dislikeCommand().setActive(disliked);
            }
        }
    }

    /// Build a fresh now-playing dictionary for the current track.
    ///
    /// # Safety
    /// Must be called on the backend's main-thread owner. Inserted values must match
    /// the Foundation object types expected by the MediaPlayer now-playing keys.
    unsafe fn build_info(
        &mut self,
        snapshot: &MediaSnapshot,
    ) -> Retained<NSMutableDictionary<NSString, AnyObject>> {
        let info = NSMutableDictionary::<NSString, AnyObject>::new();
        let Some(track) = &snapshot.track else {
            return info;
        };
        // SAFETY: keys and values are MediaPlayer/Foundation constants and objects
        // with the exact types expected by NSMutableDictionary insertion.
        unsafe {
            info.insert(MPMediaItemPropertyTitle, &*NSString::from_str(&track.title));
            if !track.artist.is_empty() {
                info.insert(
                    MPMediaItemPropertyArtist,
                    &*NSString::from_str(&track.artist),
                );
            }
            set_number(
                &info,
                MPNowPlayingInfoPropertyMediaType,
                MPNowPlayingInfoMediaType::Audio.0 as f64,
            );
            if track.is_live {
                info.insert(
                    MPNowPlayingInfoPropertyIsLiveStream,
                    &*NSNumber::numberWithBool(true),
                );
            } else if let Some(duration) = track.duration {
                set_number(&info, MPMediaItemPropertyPlaybackDuration, duration);
            }
            set_number(
                &info,
                MPNowPlayingInfoPropertyElapsedPlaybackTime,
                snapshot.position_now(),
            );
            set_number(
                &info,
                MPNowPlayingInfoPropertyPlaybackRate,
                effective_rate(snapshot),
            );
            set_number(
                &info,
                MPNowPlayingInfoPropertyDefaultPlaybackRate,
                snapshot.rate,
            );
            self.set_artwork(&info, snapshot);
        }
        info
    }

    /// Attach the cached artwork file (if any) to `info`, loading + wrapping it in
    /// an `MPMediaItemArtwork` once per file.
    ///
    /// # Safety
    /// `info` must be a mutable now-playing dictionary owned by this backend, and
    /// the retained artwork object must outlive its insertion into that dictionary.
    unsafe fn set_artwork(
        &mut self,
        info: &NSMutableDictionary<NSString, AnyObject>,
        snapshot: &MediaSnapshot,
    ) {
        let Some(path) = snapshot.track.as_ref().and_then(|t| t.art_file.clone()) else {
            return;
        };
        if self.artwork.as_ref().map(|(p, _)| p) != Some(&path) {
            let Some(artwork) = load_artwork(&path) else {
                return;
            };
            self.artwork = Some((path, artwork));
        }
        if let Some((_, artwork)) = &self.artwork {
            // SAFETY: `artwork` is retained by `self.artwork`; the key expects an
            // MPMediaItemArtwork Foundation object.
            unsafe {
                info.insert(MPMediaItemPropertyArtwork, artwork.as_ref());
            }
        }
    }
}

impl Drop for Backend {
    fn drop(&mut self) {
        // Tear the session down so quitting never leaves a ghost Now Playing entry.
        // SAFETY: `self` still owns the retained MediaPlayer objects; removing targets
        // and clearing now-playing info are valid shutdown calls for these commands.
        unsafe {
            self.center.setNowPlayingInfo(None);
            self.center
                .setPlaybackState(MPNowPlayingPlaybackState::Stopped);
            let c = &self.commands;
            let all: [&MPRemoteCommand; 11] = [
                &c.playCommand(),
                &c.pauseCommand(),
                &c.togglePlayPauseCommand(),
                &c.stopCommand(),
                &c.nextTrackCommand(),
                &c.previousTrackCommand(),
                &c.changePlaybackPositionCommand(),
                &c.changeShuffleModeCommand(),
                &c.changeRepeatModeCommand(),
                &c.likeCommand(),
                &c.dislikeCommand(),
            ];
            for command in all {
                command.removeTarget(None);
            }
        }
    }
}

/// The rate the OS should interpolate with *right now*: the playback speed while
/// playing, `0.0` while paused/stopped (spec M-2's pairing).
fn effective_rate(snapshot: &MediaSnapshot) -> f64 {
    if snapshot.status == MediaPlaybackStatus::Playing {
        snapshot.rate
    } else {
        0.0
    }
}

/// Register a handler on `command` that maps the event to a [`MediaCommand`] and
/// forwards it through the sink. Handlers must return immediately (spec C-1): the
/// actual work happens on the app's reducer thread.
///
/// # Safety
/// `command` must be a live `MPRemoteCommand` from the command center, and MediaPlayer
/// must call the block with a non-null event pointer for the block's dynamic lifetime.
unsafe fn register<F>(command: &MPRemoteCommand, sink: &CommandSink, map: F)
where
    F: Fn(&MPRemoteCommandEvent) -> Option<MediaCommand> + 'static,
{
    let sink = std::sync::Arc::clone(sink);
    let handler = RcBlock::new(
        move |event: NonNull<MPRemoteCommandEvent>| -> MPRemoteCommandHandlerStatus {
            // SAFETY: MediaPlayer invokes the block with a valid event pointer for
            // the duration of the callback; we do not store the borrowed reference.
            let event = unsafe { event.as_ref() };
            match map(event) {
                Some(cmd) => {
                    sink(cmd);
                    MPRemoteCommandHandlerStatus::Success
                }
                None => MPRemoteCommandHandlerStatus::CommandFailed,
            }
        },
    );
    // SAFETY: `command` is live for this registration call and the block is copied by
    // MediaPlayer; failure is reported by the Objective-C binding rather than UB.
    unsafe {
        command.setEnabled(true);
        let _token = command.addTargetWithHandler(&handler);
    }
}

fn set_number(info: &NSMutableDictionary<NSString, AnyObject>, key: &'static NSString, value: f64) {
    info.insert(key, &*NSNumber::numberWithDouble(value));
}

/// Load the cached artwork file into an `MPMediaItemArtwork`. The request handler
/// returns the same decoded `NSImage` for any requested size — Now Playing surfaces
/// scale it themselves, and the cache file is already display-sized (≤512²).
fn load_artwork(path: &std::path::Path) -> Option<Retained<MPMediaItemArtwork>> {
    let ns_path = NSString::from_str(path.to_str()?);
    let image = NSImage::initWithContentsOfFile(NSImage::alloc(), &ns_path)?;
    let size = image.size();
    let bounds = CGSize {
        width: if size.width > 0.0 { size.width } else { 512.0 },
        height: if size.height > 0.0 {
            size.height
        } else {
            512.0
        },
    };
    let handler = RcBlock::new(move |_wanted: CGSize| -> NonNull<NSImage> {
        // The captured `Retained` keeps the image alive for the block's lifetime.
        NonNull::from(&*image)
    });
    // SAFETY: the request handler always returns a non-null NSImage retained by the
    // copied block, and `bounds` is finite positive fallback image geometry.
    Some(unsafe {
        MPMediaItemArtwork::initWithBoundsSize_requestHandler(
            MPMediaItemArtwork::alloc(),
            bounds,
            &handler,
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn effective_rate_tracks_playing_state_only() {
        let mut snapshot = MediaSnapshot::idle();
        snapshot.rate = 1.75;

        snapshot.status = MediaPlaybackStatus::Stopped;
        assert_eq!(effective_rate(&snapshot), 0.0);

        snapshot.status = MediaPlaybackStatus::Paused;
        assert_eq!(effective_rate(&snapshot), 0.0);

        snapshot.status = MediaPlaybackStatus::Playing;
        assert_eq!(effective_rate(&snapshot), 1.75);
    }
}
