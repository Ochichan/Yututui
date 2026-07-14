use std::io;
use std::sync::atomic::{AtomicBool, Ordering};

use tokio::sync::mpsc::Receiver;

use super::MPV_IPC_MAX_LINE;
use crate::player::{EventSink, PlayerCmd, PlayerEvent};

pub(super) enum ActorExit {
    CommandChannelClosed,
    Eof,
    Read(io::Error),
    OversizedLine,
    Write {
        operation: &'static str,
        error: io::Error,
    },
    SeekCausalityLost,
    InternalCommandFailed {
        operation: &'static str,
        rejected: bool,
    },
    CacheEmergency {
        file_generation: u64,
        position_secs: f64,
        paused: bool,
        reason: crate::player::long_form_seek::CacheReason,
    },
}

impl ActorExit {
    pub(super) fn barrier_reason(&self) -> String {
        match self {
            Self::CommandChannelClosed => {
                "mpv command channel closed before acknowledgement".to_owned()
            }
            Self::Eof => "mpv IPC closed before command acknowledgement".to_owned(),
            Self::Read(error) => format!("mpv IPC read failed before acknowledgement: {error}"),
            Self::OversizedLine => {
                "mpv IPC protocol failed before command acknowledgement".to_owned()
            }
            Self::Write { operation, error } => {
                format!("mpv IPC {operation} write failed before acknowledgement: {error}")
            }
            Self::SeekCausalityLost => {
                "mpv seek completion became ambiguous before acknowledgement".to_owned()
            }
            Self::InternalCommandFailed { operation, .. } => {
                format!("mpv {operation} failed before required command acknowledgement")
            }
            Self::CacheEmergency { reason, .. } => format!(
                "managed cache safety recycle ({}) before acknowledgement",
                reason.id()
            ),
        }
    }

    fn transport_reason(self) -> Option<String> {
        let reason = match self {
            Self::CommandChannelClosed => return None,
            Self::Eof => "mpv IPC closed unexpectedly".to_owned(),
            Self::Read(error) => format!("mpv IPC read failed: {error}"),
            Self::OversizedLine => {
                format!("mpv IPC message exceeded the {MPV_IPC_MAX_LINE}-byte safety limit")
            }
            Self::Write { operation, error } => {
                format!("mpv IPC {operation} write failed: {error}")
            }
            Self::SeekCausalityLost => {
                "mpv seek completion timed out; recycling ambiguous transport".to_owned()
            }
            Self::InternalCommandFailed {
                operation,
                rejected,
            } => format!(
                "mpv {operation} {}; recycling player transport",
                if rejected {
                    "was rejected"
                } else {
                    "timed out"
                }
            ),
            Self::CacheEmergency { .. } => return None,
        };
        Some(crate::util::sanitize::sanitize_error_text(reason))
    }
}

pub(super) fn finish_actor(exit: ActorExit, emit: &EventSink) {
    crate::player::diagnostics::actor_closed();
    match exit {
        ActorExit::CacheEmergency {
            file_generation,
            position_secs,
            paused,
            reason,
        } => {
            tracing::error!(
                file_generation,
                position_secs,
                paused,
                reason = reason.id(),
                "managed cache safety boundary requires player recycle"
            );
            emit(PlayerEvent::CacheEmergency {
                file_generation,
                position_secs,
                paused,
                reason,
            });
        }
        exit => {
            if let Some(reason) = exit.transport_reason() {
                tracing::warn!(%reason, "mpv IPC transport closed");
                emit(PlayerEvent::TransportClosed(reason));
            }
        }
    }
}

pub(super) fn transport_exit_or_shutdown(
    cmd_rx: &Receiver<PlayerCmd>,
    intentional_close: &AtomicBool,
    failure: ActorExit,
) -> ActorExit {
    if intentional_close.load(Ordering::Acquire) || cmd_rx.is_closed() {
        ActorExit::CommandChannelClosed
    } else {
        failure
    }
}
