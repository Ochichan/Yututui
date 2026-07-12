//! Shared semantic coalescing for player commands waiting outside the mpv actor lane.

use std::collections::VecDeque;

use super::PlayerCmd;
use crate::util::delivery::{DeliveryError, DeliveryReceipt};

pub(crate) const PLAYER_PENDING_MAX: usize = 256;

pub(crate) struct StagedPlayerCommands {
    pub(crate) cmds: VecDeque<PlayerCmd>,
    pub(crate) receipt: DeliveryReceipt,
}

/// Add one command while preserving the barriers and ordering used by the active player
/// backlog. The caller chooses its own bound and public rejection vocabulary.
pub(crate) fn push_pending_command(
    cmds: &mut VecDeque<PlayerCmd>,
    cmd: PlayerCmd,
    capacity: usize,
    capacity_error: DeliveryError,
) -> Result<bool, DeliveryError> {
    match cmd {
        PlayerCmd::SeekRelative(secs) => match cmds.back_mut() {
            Some(PlayerCmd::SeekRelative(existing)) => {
                *existing += secs;
                Ok(true)
            }
            _ => push_new(
                cmds,
                PlayerCmd::SeekRelative(secs),
                capacity,
                capacity_error,
            ),
        },
        PlayerCmd::SeekAbsolute(secs) => match cmds.back_mut() {
            Some(PlayerCmd::SeekAbsolute(existing)) => {
                *existing = secs;
                Ok(true)
            }
            Some(PlayerCmd::SeekRelative(_)) => {
                cmds.pop_back();
                cmds.push_back(PlayerCmd::SeekAbsolute(secs));
                Ok(true)
            }
            _ => push_new(
                cmds,
                PlayerCmd::SeekAbsolute(secs),
                capacity,
                capacity_error,
            ),
        },
        PlayerCmd::SetVolume(vol) => {
            for existing in cmds.iter_mut().rev() {
                if existing.is_coalescing_barrier() {
                    break;
                }
                if matches!(existing, PlayerCmd::SetVolume(_)) {
                    *existing = PlayerCmd::SetVolume(vol);
                    return Ok(true);
                }
            }
            push_new(cmds, PlayerCmd::SetVolume(vol), capacity, capacity_error)
        }
        PlayerCmd::AfCommand {
            label,
            param,
            value,
        } => {
            for existing in cmds.iter_mut().rev() {
                if existing.is_coalescing_barrier() {
                    break;
                }
                if matches!(
                    existing,
                    PlayerCmd::AfCommand {
                        label: existing_label,
                        param: existing_param,
                        ..
                    } if existing_label == &label && existing_param == &param
                ) {
                    *existing = PlayerCmd::AfCommand {
                        label,
                        param,
                        value,
                    };
                    return Ok(true);
                }
            }
            push_new(
                cmds,
                PlayerCmd::AfCommand {
                    label,
                    param,
                    value,
                },
                capacity,
                capacity_error,
            )
        }
        PlayerCmd::CyclePause if matches!(cmds.back(), Some(PlayerCmd::CyclePause)) => {
            cmds.pop_back();
            Ok(true)
        }
        // Each accepted Load owns a reducer commit (history, queue cursor, signals, remote
        // acknowledgement). Replacing it with a later Load would revoke already-committed
        // work and create a track that existed in state but was never visible to mpv.
        PlayerCmd::Load(url) => push_new(cmds, PlayerCmd::Load(url), capacity, capacity_error),
        cmd => push_new(cmds, cmd, capacity, capacity_error),
    }
}

/// Stage a batch privately and publish only its final semantic form. Intermediate growth may
/// exceed the bound when a later command cancels or coalesces it back under the limit.
pub(crate) fn stage_pending_batch(
    current: &VecDeque<PlayerCmd>,
    batch: Vec<PlayerCmd>,
    capacity: usize,
    capacity_error: DeliveryError,
) -> Result<StagedPlayerCommands, DeliveryError> {
    if batch.is_empty() {
        return Ok(StagedPlayerCommands {
            cmds: current.clone(),
            receipt: DeliveryReceipt::Enqueued,
        });
    }

    let initial_len = current.len();
    let mut staged = current.clone();
    let mut any_coalesced = false;
    for cmd in batch {
        any_coalesced |= push_pending_command(&mut staged, cmd, usize::MAX, capacity_error)?;
    }
    if staged.len() > capacity {
        return Err(capacity_error);
    }

    let receipt = if staged.len() > initial_len {
        DeliveryReceipt::Deferred
    } else {
        DeliveryReceipt::Coalesced {
            replaced_existing: any_coalesced,
            evicted_oldest: false,
        }
    };
    Ok(StagedPlayerCommands {
        cmds: staged,
        receipt,
    })
}

fn push_new(
    cmds: &mut VecDeque<PlayerCmd>,
    cmd: PlayerCmd,
    capacity: usize,
    capacity_error: DeliveryError,
) -> Result<bool, DeliveryError> {
    if cmds.len() >= capacity {
        return Err(capacity_error);
    }
    cmds.push_back(cmd);
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn tracked(barrier: &crate::util::command_barrier::CommandBarrier) -> PlayerCmd {
        PlayerCmd::tracked_property(
            "stream-record".to_owned(),
            serde_json::Value::from("next.mkv"),
            barrier,
        )
    }

    #[test]
    fn tracked_property_is_never_coalesced() {
        let first = crate::util::command_barrier::CommandBarrier::pending();
        let second = crate::util::command_barrier::CommandBarrier::pending();
        let mut pending = VecDeque::new();
        assert!(
            !push_pending_command(&mut pending, tracked(&first), 2, DeliveryError::Busy).unwrap()
        );
        assert!(
            !push_pending_command(&mut pending, tracked(&second), 2, DeliveryError::Busy).unwrap()
        );
        assert_eq!(pending.len(), 2);
    }

    #[test]
    fn staging_rejection_and_queue_clear_fail_exact_barriers() {
        let rejected = crate::util::command_barrier::CommandBarrier::pending();
        assert!(
            stage_pending_batch(
                &VecDeque::new(),
                vec![tracked(&rejected)],
                0,
                DeliveryError::Busy,
            )
            .is_err()
        );
        assert!(
            rejected
                .wait_for_test(Duration::ZERO)
                .unwrap_err()
                .contains("dropped before acknowledgement")
        );

        let cleared = crate::util::command_barrier::CommandBarrier::pending();
        let mut pending = VecDeque::from([tracked(&cleared)]);
        pending.clear();
        assert!(
            cleared
                .wait_for_test(Duration::ZERO)
                .unwrap_err()
                .contains("dropped before acknowledgement")
        );
    }

    #[tokio::test]
    async fn channel_close_drops_tracked_command_and_fails_barrier() {
        let barrier = crate::util::command_barrier::CommandBarrier::pending();
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        tx.send(tracked(&barrier)).await.unwrap();
        drop(rx);
        assert!(
            barrier
                .wait_for_test(Duration::ZERO)
                .unwrap_err()
                .contains("dropped before acknowledgement")
        );
    }
}
