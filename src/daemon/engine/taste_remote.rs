use super::*;
use crate::queue::QueueRemovalPlayback;
use crate::remote::proto::BanTarget;
use crate::streaming::TasteEdit;

impl DaemonEngine {
    pub(super) async fn ban_current(
        &mut self,
        target: BanTarget,
    ) -> (RemoteResponse, Vec<EngineEffect>) {
        if !self.streaming_active() {
            return (RemoteResponse::err("not_streaming"), Vec::new());
        }
        let Some(song) = self.queue.current().cloned() else {
            return (RemoteResponse::err("no_current_track"), Vec::new());
        };
        let edit = match target {
            BanTarget::Track => match TasteEdit::ban_track(&song) {
                Ok(edit) => edit,
                Err(_) => return (RemoteResponse::err("no_track_id"), Vec::new()),
            },
            BanTarget::Artist => match TasteEdit::ban_artist(&song) {
                Ok(edit) => edit,
                Err(_) => return (RemoteResponse::err("no_artist"), Vec::new()),
            },
        };

        self.cancel_pending_streaming_request();
        let Some((mut mutation, outcome)) = self.queue.prepare_purge(|queued| edit.rejects(queued))
        else {
            let _ = self.taste.apply(edit);
            return (
                RemoteResponse::ok("ban applied".to_owned()),
                self.force_autoplay_extend(),
            );
        };

        let playback = match outcome.playback() {
            QueueRemovalPlayback::LoadSelected => mutation.select_first_playable(|song| {
                song.unplayable_youtube_ref_reason().is_none()
                    && song.playback_destination_checked().is_ok()
            }),
            playback => playback,
        };
        let outgoing = (playback != QueueRemovalPlayback::Unchanged)
            .then(|| self.prepare_outgoing(false))
            .flatten();
        let detached = (outcome.playback() == QueueRemovalPlayback::Stop).then_some(song);
        let previous = self.queue.snapshot();
        self.queue.commit_mutation(mutation);
        let response = match playback {
            QueueRemovalPlayback::Unchanged => Ok(()),
            QueueRemovalPlayback::LoadSelected => {
                self.load_current_or_restore_queue(previous).await
            }
            QueueRemovalPlayback::Stop => {
                self.stop_playback();
                self.save_session();
                Ok(())
            }
        };
        if let Err(error) = response {
            return (RemoteResponse::err(error.reason()), Vec::new());
        }
        if let Some(outgoing) = outgoing {
            self.commit_outgoing(outgoing);
        }
        let _ = self.taste.apply(edit);
        if outcome.playback() == QueueRemovalPlayback::Unchanged {
            self.save_session();
        }
        let effects = match detached {
            Some(seed) => self.force_autoplay_extend_from(&seed),
            None => self.force_autoplay_extend(),
        };
        (RemoteResponse::ok("ban applied".to_owned()), effects)
    }
}
