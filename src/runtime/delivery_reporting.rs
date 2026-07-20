use crate::app::{App, Msg, StreamingMsg};
use crate::util::delivery::DeliveryResult;

pub(super) fn report_actor_delivery(
    app: &mut App,
    component: &'static str,
    result: DeliveryResult,
) -> bool {
    match result {
        Ok(receipt) => {
            tracing::trace!(component, ?receipt, "actor command accepted");
            true
        }
        Err(error) => {
            tracing::warn!(component, %error, "actor command was not accepted");
            app.set_status_error(format!(
                "{}: {error}",
                crate::t!(
                    "Background service is busy",
                    "백그라운드 서비스가 바쁩니다",
                    "バックグラウンドサービスがビジー状態です"
                )
            ));
            false
        }
    }
}

pub(super) enum ActorRejectionRecovery {
    Lyrics,
    Artwork,
    AiTurn,
    AiRerank {
        request_id: u64,
        seed_video_id: String,
    },
    AiFeedback,
    TransferStart,
    TransferCancel,
}

pub(super) fn recover_actor_rejection(
    app: &mut App,
    recovery: ActorRejectionRecovery,
) -> Option<Msg> {
    app.dirty = true;
    match recovery {
        ActorRejectionRecovery::Lyrics => app.lyrics.loading = false,
        ActorRejectionRecovery::Artwork => app.art.loading = false,
        ActorRejectionRecovery::AiTurn => app.ai.thinking = false,
        ActorRejectionRecovery::AiRerank {
            request_id,
            seed_video_id,
        } => {
            app.ai.thinking = false;
            return Some(Msg::Streaming(StreamingMsg::AiPicks {
                request_id,
                seed_video_id,
                picks: Vec::new(),
                conf: None,
            }));
        }
        ActorRejectionRecovery::AiFeedback => app.streaming.feedback_in_flight = false,
        ActorRejectionRecovery::TransferStart => app.transfer_running = false,
        ActorRejectionRecovery::TransferCancel => app.transfer_running = true,
    }
    None
}
