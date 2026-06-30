//! Remote-control command application.
//!
//! Maps a [`RemoteCommand`] onto the **same** reducer paths a keypress uses
//! ([`App::on_player_action`], [`App::maybe_autoplay_extend`], [`App::quit_app`]), so
//! `ytt -r <cmd>` is mode-independent: `ytt -r next` skips a track even while the TUI is in
//! Search text entry or Settings. Each command also produces a [`RemoteResponse`] computed
//! from the resulting state, which the control socket writes back to the client.

use super::*;
use crate::remote::proto::{RemoteCommand, RemoteResponse, StatusSnapshot, ToggleState};

impl App {
    /// Apply one remote command and return `(response, side-effect commands)`. The commands
    /// flow through the normal run-loop dispatch exactly as a keypress's would.
    pub(in crate::app) fn apply_remote(
        &mut self,
        cmd: RemoteCommand,
    ) -> (RemoteResponse, Vec<Cmd>) {
        match cmd {
            RemoteCommand::Next => {
                let cmds = self.on_player_action(Action::NextTrack);
                (self.transport_resp(), cmds)
            }
            RemoteCommand::Prev => {
                let cmds = self.on_player_action(Action::PrevTrack);
                (self.transport_resp(), cmds)
            }
            RemoteCommand::TogglePause => {
                let cmds = self.on_player_action(Action::TogglePause);
                (RemoteResponse::ok(self.pause_line()), cmds)
            }
            RemoteCommand::VolumeUp => {
                let cmds = self.on_player_action(Action::VolUp);
                (RemoteResponse::ok(self.vol_line()), cmds)
            }
            RemoteCommand::VolumeDown => {
                let cmds = self.on_player_action(Action::VolDown);
                (RemoteResponse::ok(self.vol_line()), cmds)
            }
            RemoteCommand::SeekBack => {
                let cmds = self.on_player_action(Action::SeekBack);
                (RemoteResponse::ok(self.now_playing_line()), cmds)
            }
            RemoteCommand::SeekForward => {
                let cmds = self.on_player_action(Action::SeekForward);
                (RemoteResponse::ok(self.now_playing_line()), cmds)
            }
            RemoteCommand::Radio { state } => self.remote_set_radio(state),
            RemoteCommand::Status => (RemoteResponse::status(self.status_snapshot()), Vec::new()),
            RemoteCommand::Quit => {
                let cmds = self.quit_app();
                (RemoteResponse::ok("quitting ytt".to_string()), cmds)
            }
        }
    }

    /// Set/toggle autoplay radio, mirroring the `ToggleRadio` key handler (status toast +
    /// an immediate top-up when enabling, so a low queue doesn't gap before the next track).
    fn remote_set_radio(&mut self, state: ToggleState) -> (RemoteResponse, Vec<Cmd>) {
        let on = state.resolve(self.autoplay_radio);
        self.autoplay_radio = on;
        self.status.text = format!(
            "{}: {}",
            t!("Autoplay radio", "자동재생 라디오"),
            if on { "✓" } else { "✗" }
        );
        self.dirty = true;
        let mut cmds = vec![self.save_playback_modes_cmd()];
        if on {
            cmds.extend(self.maybe_autoplay_extend());
        }
        (
            RemoteResponse::ok(format!("radio {}", if on { "on" } else { "off" })),
            cmds,
        )
    }

    /// A transport response: the now-playing line on success, or `queue_empty` when nothing
    /// is loaded (so `ytt -r next` on an empty queue is a clean rejection, not a fake OK).
    fn transport_resp(&self) -> RemoteResponse {
        if self.queue.current().is_some() {
            RemoteResponse::ok(self.now_playing_line())
        } else {
            RemoteResponse::err("queue_empty")
        }
    }

    fn now_playing_line(&self) -> String {
        match self.queue.current() {
            Some(s) => self.display_song_label(s),
            None => "nothing playing".to_string(),
        }
    }

    fn pause_line(&self) -> String {
        let state = if self.playback.paused {
            "paused"
        } else {
            "playing"
        };
        match self.queue.current() {
            Some(s) => format!("{state}: {}", self.display_song_label(s)),
            None => state.to_string(),
        }
    }

    fn vol_line(&self) -> String {
        format!("volume {}%", self.playback.volume)
    }

    fn status_snapshot(&self) -> StatusSnapshot {
        let cur = self.queue.current();
        let (position, total) = self.queue.position();
        StatusSnapshot {
            title: cur.map(|s| self.display_title(s).into_owned()),
            artist: cur.map(|s| self.display_artist(s).into_owned()),
            paused: self.playback.paused,
            volume: self.playback.volume,
            position: if total == 0 { 0 } else { position },
            total,
            radio: self.autoplay_radio,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::Song;

    fn two_track_app() -> App {
        let mut app = App::new(50);
        app.queue.set(
            vec![
                Song::remote("id0", "Zero", "A", "3:00"),
                Song::remote("id1", "One", "B", "3:00"),
            ],
            0,
        );
        app
    }

    #[test]
    fn next_advances_even_in_search_mode() {
        let mut app = two_track_app();
        // The whole point of routing through the reducer (not key replay): a non-player
        // input mode must not swallow the command as text.
        app.mode = Mode::Search;
        let (resp, _cmds) = app.apply_remote(RemoteCommand::Next);
        assert!(resp.ok);
        assert_eq!(app.queue.current().unwrap().video_id, "id1");
    }

    #[test]
    fn next_on_empty_queue_is_rejected() {
        let mut app = App::new(50);
        let (resp, _cmds) = app.apply_remote(RemoteCommand::Next);
        assert!(!resp.ok);
        assert_eq!(resp.reason.as_deref(), Some("queue_empty"));
    }

    #[test]
    fn radio_on_off_toggle_set_autoplay_radio() {
        let mut app = App::new(50);
        app.mode = Mode::Settings; // mode-independent
        assert!(!app.autoplay_radio);

        let (resp, _) = app.apply_remote(RemoteCommand::Radio {
            state: ToggleState::On,
        });
        assert!(resp.ok);
        assert!(app.autoplay_radio);

        app.apply_remote(RemoteCommand::Radio {
            state: ToggleState::Off,
        });
        assert!(!app.autoplay_radio);

        app.apply_remote(RemoteCommand::Radio {
            state: ToggleState::Toggle,
        });
        assert!(app.autoplay_radio);
    }

    #[test]
    fn quit_sets_should_quit() {
        let mut app = App::new(50);
        assert!(!app.should_quit);
        let (resp, _) = app.apply_remote(RemoteCommand::Quit);
        assert!(resp.ok);
        assert!(app.should_quit);
    }

    #[test]
    fn volume_up_raises_volume_and_reports_it() {
        let mut app = App::new(50);
        let before = app.playback.volume;
        let (resp, _) = app.apply_remote(RemoteCommand::VolumeUp);
        assert!(resp.ok);
        assert!(app.playback.volume > before);
        assert!(resp.message.unwrap().contains("volume"));
    }

    #[test]
    fn status_reports_queue_and_radio() {
        let mut app = two_track_app();
        app.autoplay_radio = true;
        let (resp, cmds) = app.apply_remote(RemoteCommand::Status);
        assert!(cmds.is_empty());
        let snap = resp.status.expect("status snapshot present");
        assert_eq!(snap.total, 2);
        assert_eq!(snap.position, 1);
        assert!(snap.radio);
        assert_eq!(snap.title.as_deref(), Some("Zero"));
    }
}
