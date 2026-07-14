//! Mouse actions for frame-validated synced-lyric targets.

use super::*;

impl App {
    pub(in crate::app) fn on_lyrics_mouse_target(&mut self, target: MouseTarget) -> Vec<Cmd> {
        match target {
            MouseTarget::LyricsLine {
                video_id,
                line_index,
            } => {
                let Some(position) = self.lyrics_line_seek_target(video_id.as_ref(), line_index)
                else {
                    return Vec::new();
                };
                self.player_intent(
                    "seek_absolute",
                    PlayerCmd::exact_seek(position),
                    PlayerCommit::Seek {
                        optimistic_position: Some(position),
                    },
                )
            }
            MouseTarget::LyricsDelayHandle { video_id } => {
                let current = self
                    .current_loaded_lyrics()
                    .is_some_and(|track| track.video_id == video_id);
                if current && self.reopen_lyrics_delay_osd(Instant::now()) {
                    self.dirty = true;
                }
                Vec::new()
            }
            MouseTarget::LyricsDelayEarlier { video_id } => {
                if self
                    .current_loaded_lyrics()
                    .is_some_and(|track| track.video_id == video_id)
                {
                    self.on_player_action(Action::LyricsDelayEarlier)
                } else {
                    Vec::new()
                }
            }
            MouseTarget::LyricsDelayLater { video_id } => {
                if self
                    .current_loaded_lyrics()
                    .is_some_and(|track| track.video_id == video_id)
                {
                    self.on_player_action(Action::LyricsDelayLater)
                } else {
                    Vec::new()
                }
            }
            MouseTarget::LyricsDelayBlock => Vec::new(),
            _ => Vec::new(),
        }
    }
}
