//! Daemon owner for chapter jumps. The App mirrors this in `App::try_jump_chapter`; both
//! resolve the target through `yututui_core::chapters`.

use yututui_core::chapters::{self, ChapterDirection};

use super::*;

impl DaemonEngine {
    pub(super) fn loaded_chapters(&self) -> &[crate::player::Chapter] {
        crate::player::chapters_for_track(
            &self.chapters,
            self.chapters_track.as_deref(),
            self.loaded_video_id.as_deref(),
        )
    }

    pub(super) fn jump_chapter(&mut self, direction: ChapterDirection) -> RemoteResponse {
        if self.loaded_video_id.is_none() {
            return RemoteResponse::err("nothing_playing");
        }
        let position = self.playback.time_pos.unwrap_or(0.0);
        let starts = self
            .loaded_chapters()
            .iter()
            .map(|chapter| chapter.start_secs);
        let target = match chapters::jump_target(starts, position, direction) {
            Ok(target) => target,
            Err(refusal) => return RemoteResponse::err(refusal.reason()),
        };
        if let Err(error) =
            self.send_active_player_command("seek_chapter", PlayerCmd::interactive_seek(target))
        {
            return self.reject_player_command(error);
        }
        self.note_seek(target);
        RemoteResponse::status(self.status())
    }
}
