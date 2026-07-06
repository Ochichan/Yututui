//! Radio recorder state machine (a Shortwave-style feature), split out of the monolithic
//! `app.rs`. Pure in-memory transitions driven by the ICY-title diff in `PlayerMsg::Metadata`
//! and the 1 Hz `Msg::RecordingTick`; the actual disk work is emitted as `Cmd::Recorder` jobs
//! (run off the loop) and mpv writes the audio itself via the `stream-record` property.
//!
//! Rotation model: keep a rolling recording so the *next* track is captured from its start.
//! On each real title change we finalize the segment we were writing (the track that just
//! ended, whose title we knew) and open a fresh one for the new title. The very first segment
//! after tuning in was joined mid-song, so it is flagged incomplete and dropped — exactly like
//! Shortwave.

use std::path::Path;
use std::time::Instant;

use super::*;

use crate::player::PlayerCmd;
use crate::recorder::job::RecorderJob;
use crate::recorder::{
    OpenSegment, RecordedTrack, RecordingMode, RecordingState, codec_to_ext, track_filename_base,
};

impl App {
    /// A segment is currently being written (drives the guarded recording tick).
    pub fn recorder_active(&self) -> bool {
        self.recorder.current.is_some()
    }

    /// Recording should be running right now: mpv supports it, a mode is selected, and the
    /// active track is a radio stream.
    fn recorder_enabled(&self) -> bool {
        self.recorder.supported
            && !self.config.recording.mode.is_off()
            && self.current_is_radio_stream()
    }

    fn current_station_name(&self) -> Option<String> {
        self.queue
            .current()
            .filter(|s| s.is_radio_station())
            .map(|s| s.title.clone())
    }

    /// The ICY title changed (to `new`, or `None` for an ad / station-ID / lost metadata).
    /// Finalize the segment we were writing, then — for a real title — open the next one.
    pub(in crate::app) fn recorder_on_title(&mut self, new: Option<&StreamNowPlaying>) -> Vec<Cmd> {
        if !self.recorder_enabled() {
            // Recording was turned off (or we left radio) while a segment was open: stop it.
            if self.recorder.current.is_some() {
                return self.recorder_teardown();
            }
            return Vec::new();
        }
        let mut cmds = self.recorder_finalize(false);
        if let Some(np) = new {
            let incomplete = !self.recorder.saw_first_title;
            self.recorder.saw_first_title = true;
            cmds.extend(self.recorder_open_segment(np, incomplete));
        }
        cmds
    }

    /// 1 Hz while recording: force-split a track that has run past the max duration. The audio
    /// is kept; a fresh segment reopens for the same title (inheriting incomplete-ness).
    pub(in crate::app) fn recorder_on_tick(&mut self) -> Vec<Cmd> {
        let Some(seg) = self.recorder.current.as_ref() else {
            return Vec::new();
        };
        if (seg.started_at.elapsed().as_secs() as u32) < self.config.effective_recording_max() {
            return Vec::new();
        }
        // Snapshot the label + incomplete flag before finalize consumes the segment.
        let np = StreamNowPlaying {
            title: seg.title.clone(),
            artist: seg.artist.clone(),
            raw: seg.raw.clone(),
        };
        let incomplete = seg.incomplete;
        let mut cmds = self.recorder_finalize(true);
        if self.recorder_enabled() {
            cmds.extend(self.recorder_open_segment(&np, incomplete));
        }
        cmds
    }

    /// Open a fresh recording segment for `np` and point mpv's `stream-record` at its temp file.
    fn recorder_open_segment(&mut self, np: &StreamNowPlaying, incomplete: bool) -> Vec<Cmd> {
        let ext = codec_to_ext(
            self.playback.audio_codec.as_deref(),
            self.playback.file_format.as_deref(),
        );
        let station = self.current_station_name();
        let (id, temp_path) = self.recorder.next_temp(ext);
        let cmd = set_stream_record(&temp_path);
        self.recorder.current = Some(OpenSegment {
            id,
            temp_path,
            title: np.title.clone(),
            artist: np.artist.clone(),
            raw: np.raw.clone(),
            station,
            started_at: Instant::now(),
            incomplete,
            ext,
        });
        self.dirty = true;
        vec![cmd]
    }

    /// Close the open segment: stop mpv writing, then drop it (incomplete / too short) or keep
    /// it as a finished track (pushing it into the browser history, auto-saving in Everything
    /// mode). `reached_max` keeps the audio regardless of the minimum-duration filter.
    pub(in crate::app) fn recorder_finalize(&mut self, reached_max: bool) -> Vec<Cmd> {
        let Some(seg) = self.recorder.current.take() else {
            return Vec::new();
        };
        let mut cmds = vec![clear_stream_record()];
        let dur = seg.started_at.elapsed().as_secs() as u32;
        self.dirty = true;

        let below_min = !reached_max && dur < self.config.effective_recording_min();
        if seg.incomplete || below_min {
            cmds.push(Cmd::Recorder(RecorderJob::Discard {
                temp: seg.temp_path,
            }));
            return cmds;
        }

        let track = RecordedTrack {
            id: seg.id,
            title: seg.title,
            artist: seg.artist,
            raw: seg.raw,
            station: seg.station,
            temp_path: seg.temp_path,
            ext: seg.ext,
            duration_secs: dur,
            state: if reached_max {
                RecordingState::RecordedReachedMaxDuration
            } else {
                RecordingState::Recorded
            },
            final_path: None,
        };
        cmds.extend(self.recorder_push_history(track));

        // Everything mode: save every kept track straight away.
        if matches!(self.config.recording.mode, RecordingMode::Everything) {
            let dir = self.config.effective_recording_dir();
            if let Some(front) = self.recorder.history.front_mut() {
                front.state = RecordingState::Saved; // optimistic; reverted on SaveFailed
                cmds.push(save_cmd_for(front, &dir));
            }
        }
        cmds
    }

    /// Push a finished track to the front of the bounded history, de-duplicating a consecutive
    /// same-title unsaved entry (station re-sent the title / a flap) and evicting past the cap.
    /// Returns discard jobs for any temp file that is no longer reachable.
    fn recorder_push_history(&mut self, track: RecordedTrack) -> Vec<Cmd> {
        let mut cmds = Vec::new();
        let dup = self.recorder.history.front().is_some_and(|front| {
            front.title == track.title
                && front.artist == track.artist
                && front.raw == track.raw
                && !matches!(front.state, RecordingState::Saved)
        });
        if dup && let Some(old) = self.recorder.history.pop_front() {
            cmds.push(Cmd::Recorder(RecorderJob::Discard {
                temp: old.temp_path,
            }));
        }
        self.recorder.history.push_front(track);

        let cap = self.config.effective_recording_past_tracks();
        while self.recorder.history.len() > cap {
            if let Some(old) = self.recorder.history.pop_back() {
                // The saved copy (final_path) survives; only the temp is reclaimed.
                cmds.push(Cmd::Recorder(RecorderJob::Discard {
                    temp: old.temp_path,
                }));
            }
        }
        cmds
    }

    /// Cut recording immediately (leaving radio, stopping, or app quit). The open segment is
    /// mid-song → dropped; the next stream's first title starts fresh as incomplete.
    pub(in crate::app) fn recorder_teardown(&mut self) -> Vec<Cmd> {
        self.recorder.saw_first_title = false;
        if self.recorder.current.is_none() {
            return Vec::new();
        }
        if let Some(seg) = self.recorder.current.as_mut() {
            seg.incomplete = true; // force the drop path
        }
        self.recorder_finalize(false)
    }

    /// Save a kept history track (Decide-mode "save"), or cancel the in-progress recording when
    /// `id` is the open segment.
    pub(in crate::app) fn recorder_save(&mut self, id: u64) -> Vec<Cmd> {
        let dir = self.config.effective_recording_dir();
        if let Some(track) = self.recorder.history.iter_mut().find(|t| t.id == id)
            && track.state.is_recorded()
        {
            track.state = RecordingState::Saved; // optimistic
            self.dirty = true;
            return vec![save_cmd_for(track, &dir)];
        }
        Vec::new()
    }

    /// Discard a track: cancel it if it is the in-progress segment, else remove it from the
    /// browser (deleting its temp; an already-saved final file is kept).
    pub(in crate::app) fn recorder_discard(&mut self, id: u64) -> Vec<Cmd> {
        if self.recorder.current.as_ref().is_some_and(|s| s.id == id) {
            if let Some(seg) = self.recorder.current.as_mut() {
                seg.incomplete = true;
            }
            return self.recorder_finalize(false);
        }
        if let Some(pos) = self.recorder.history.iter().position(|t| t.id == id)
            && let Some(track) = self.recorder.history.remove(pos)
        {
            self.dirty = true;
            return vec![Cmd::Recorder(RecorderJob::Discard {
                temp: track.temp_path,
            })];
        }
        Vec::new()
    }

    /// Ids addressable in the recordings browser: the in-progress segment (if any) first,
    /// then the finished-track history (most-recent first).
    pub(in crate::app) fn recordings_browser_ids(&self) -> Vec<u64> {
        let mut ids = Vec::new();
        if let Some(seg) = self.recorder.current.as_ref() {
            ids.push(seg.id);
        }
        ids.extend(self.recorder.history.iter().map(|t| t.id));
        ids
    }

    /// Open a saved recording with the OS default handler; a not-yet-saved track hints to save.
    pub(in crate::app) fn recorder_reveal(&mut self, id: u64) {
        let path = self
            .recorder
            .history
            .iter()
            .find(|t| t.id == id)
            .and_then(|t| t.final_path.clone());
        match path {
            Some(p) => crate::util::browser::open_path(&p),
            None => {
                self.status.kind = StatusKind::Info;
                self.status.text = if crate::i18n::is_korean() {
                    "재생하려면 먼저 저장하세요".to_owned()
                } else {
                    "Save the track first to play it".to_owned()
                };
                self.dirty = true;
            }
        }
    }

    /// A recorder disk job finished: update the history row and toast (if enabled).
    pub(in crate::app) fn on_recorder_event(
        &mut self,
        ev: crate::recorder::job::RecorderEvent,
    ) -> Vec<Cmd> {
        use crate::recorder::job::RecorderEvent;
        let mut cmds = Vec::new();
        match ev {
            RecorderEvent::Saved { id, final_path } => {
                if let Some(track) = self.recorder.history.iter_mut().find(|t| t.id == id) {
                    track.state = RecordingState::Saved;
                    track.final_path = Some(final_path.clone());
                }
                if self.config.recording.notify {
                    let name = final_path
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    let (title, body) = if crate::i18n::is_korean() {
                        ("녹음 저장됨".to_owned(), name.clone())
                    } else {
                        ("Recording saved".to_owned(), name.clone())
                    };
                    // In-app toast (always visible in the terminal) + a real desktop notification
                    // (OSC / native, resolved in the main loop). The toast is the final fallback.
                    self.status.kind = StatusKind::Info;
                    self.status.text = format!("{title}: {name}");
                    cmds.push(Cmd::DesktopNotify { title, body });
                }
                self.dirty = true;
            }
            RecorderEvent::SaveFailed { id, error } => {
                if let Some(track) = self.recorder.history.iter_mut().find(|t| t.id == id)
                    && matches!(track.state, RecordingState::Saved)
                {
                    track.state = RecordingState::Recorded; // let the user retry
                }
                self.status.kind = StatusKind::Error;
                self.status.text = if crate::i18n::is_korean() {
                    format!("녹음 저장 실패: {error}")
                } else {
                    format!("Recording save failed: {error}")
                };
                self.dirty = true;
            }
        }
        cmds
    }
}

/// mpv command: start writing the live stream to `path` (passthrough, native codec).
fn set_stream_record(path: &Path) -> Cmd {
    Cmd::Player(PlayerCmd::SetProperty {
        name: "stream-record".to_owned(),
        value: serde_json::Value::from(path.to_string_lossy().into_owned()),
    })
}

/// mpv command: stop writing and finalize the current file.
fn clear_stream_record() -> Cmd {
    Cmd::Player(PlayerCmd::SetProperty {
        name: "stream-record".to_owned(),
        value: serde_json::Value::from(""),
    })
}

/// Build the off-loop copy+tag job for a kept track.
fn save_cmd_for(track: &RecordedTrack, dir: &Path) -> Cmd {
    Cmd::Recorder(RecorderJob::Save {
        id: track.id,
        temp: track.temp_path.clone(),
        final_dir: dir.to_path_buf(),
        filename: track_filename_base(track.title.as_deref(), &track.raw),
        ext: track.ext,
        title: track.title.clone(),
        artist: track.artist.clone(),
        station: track.station.clone(),
    })
}
