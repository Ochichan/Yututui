//! mpv audio-output discovery and the correlated device switch/rollback transaction.

use std::io;
use std::time::Instant;

use interprocess::local_socket::tokio::Stream;
use serde_json::Value;
use tokio::time::Duration;

use super::{DispatchState, EventSink, PendingCommand, write_json};
use crate::player::audio::{
    normalize_audio_device_request, normalize_current_audio_output,
    normalize_selected_audio_device, parse_audio_device_list, sanitize_audio_error_text,
};
use crate::player::proto;
use crate::player::{PlayerCmd, PlayerEvent};

const SELECTION_TIMEOUT: Duration = Duration::from_secs(5);

/// One device switch owns the player command lane until its final reply arrives.
#[derive(Default)]
pub(super) struct State {
    pub(super) selection_request_id: Option<u64>,
    pub(super) selection_deadline: Option<Instant>,
    pub(super) followup: Option<Selection>,
    /// Exact native value of `options/ao` before this transaction changed it.
    pub(super) prior_output: Option<Value>,
    /// Device-selection rejection retained until the forced output rollback is acknowledged.
    pub(super) selection_error: Option<String>,
}

impl State {
    pub(super) fn is_idle(&self) -> bool {
        self.selection_request_id.is_none()
    }

    pub(super) fn tokio_deadline(&self) -> Option<tokio::time::Instant> {
        self.selection_deadline.map(tokio::time::Instant::from_std)
    }
}

pub(super) enum Pending {
    Refresh,
    Selection(Selection),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum SelectionPhase {
    InspectOutput,
    ClearOutput,
    SetDevice,
    RestoreOutput,
}

#[derive(Debug)]
pub(super) struct Selection {
    pub(super) correlation_id: u64,
    pub(super) device: Option<String>,
    pub(super) phase: SelectionPhase,
}

pub(super) fn is_command(cmd: &PlayerCmd) -> bool {
    matches!(
        cmd,
        PlayerCmd::RefreshAudioDevices | PlayerCmd::SelectAudioDevice { .. }
    )
}

pub(super) async fn dispatch_command(
    conn: &Stream,
    emit: &EventSink,
    state: &mut DispatchState,
    command_request_id: u64,
    cmd: PlayerCmd,
) -> io::Result<()> {
    match cmd {
        PlayerCmd::RefreshAudioDevices => {
            dispatch_refresh(conn, emit, state, command_request_id).await
        }
        PlayerCmd::SelectAudioDevice {
            correlation_id,
            device,
        } => {
            dispatch_selection(
                conn,
                emit,
                state,
                command_request_id,
                correlation_id,
                device,
            )
            .await
        }
        _ => unreachable!("caller routes only audio-output commands"),
    }
}

pub(super) async fn perform_followup(
    conn: &Stream,
    emit: &EventSink,
    state: &mut DispatchState,
    request_id: &mut u64,
    selection: Selection,
) -> io::Result<()> {
    *request_id = request_id.wrapping_add(1);
    let command_request_id = *request_id;
    let json = selection_command(state, &selection, command_request_id);
    if let Err(selection) = remember_selection(state, command_request_id, selection) {
        finish_selection(state);
        emit_selection_result(
            emit,
            &selection,
            Err("mpv acknowledgement queue saturated".to_owned()),
        );
        return Ok(());
    }
    state.audio_output.selection_request_id = Some(command_request_id);
    write_json(conn, &json).await
}

pub(super) fn dispatch_property(name: &str, value: &Value, emit: &EventSink) -> bool {
    let event = match name {
        "audio-device-list" => PlayerEvent::AudioDeviceList(parse_audio_device_list(value)),
        "audio-device" => PlayerEvent::AudioDeviceChanged(normalize_selected_audio_device(value)),
        "current-ao" => PlayerEvent::CurrentAudioOutput(normalize_current_audio_output(value)),
        _ => return false,
    };
    emit(event);
    true
}

pub(super) fn dispatch_reply(
    state: &mut DispatchState,
    pending: &mut PendingCommand,
    error: &str,
    data: Option<&Value>,
    emit: &EventSink,
) -> bool {
    let Some(pending) = pending.audio_output.take() else {
        return false;
    };
    let succeeded = error == "success" || error.is_empty();
    match pending {
        Pending::Refresh if succeeded => emit(PlayerEvent::AudioDeviceList(
            parse_audio_device_list(data.unwrap_or(&Value::Null)),
        )),
        Pending::Refresh => emit(PlayerEvent::AudioDeviceRefreshFailed(
            sanitize_audio_error_text(format!("mpv could not refresh audio devices ({error})")),
        )),
        Pending::Selection(mut selection) => match selection.phase {
            SelectionPhase::InspectOutput if succeeded => {
                state.audio_output.prior_output = forced_output_value(data);
                selection.phase = if state.audio_output.prior_output.is_some() {
                    SelectionPhase::ClearOutput
                } else {
                    SelectionPhase::SetDevice
                };
                state.audio_output.followup = Some(selection);
            }
            SelectionPhase::InspectOutput => {
                finish_selection(state);
                emit_selection_result(
                    emit,
                    &selection,
                    Err(sanitize_audio_error_text(format!(
                        "mpv could not inspect the forced audio output ({error})"
                    ))),
                );
            }
            SelectionPhase::ClearOutput if succeeded => {
                selection.phase = SelectionPhase::SetDevice;
                state.audio_output.followup = Some(selection);
            }
            SelectionPhase::ClearOutput => {
                finish_selection(state);
                emit_selection_result(
                    emit,
                    &selection,
                    Err(sanitize_audio_error_text(format!(
                        "mpv could not clear the forced audio output ({error})"
                    ))),
                );
            }
            SelectionPhase::SetDevice if succeeded => {
                finish_selection(state);
                emit_selection_result(emit, &selection, Ok(()));
            }
            SelectionPhase::SetDevice => {
                let selection_error =
                    sanitize_audio_error_text(format!("mpv rejected the audio device ({error})"));
                if state.audio_output.prior_output.is_some() {
                    state.audio_output.selection_error = Some(selection_error);
                    selection.phase = SelectionPhase::RestoreOutput;
                    state.audio_output.followup = Some(selection);
                } else {
                    finish_selection(state);
                    emit_selection_result(emit, &selection, Err(selection_error));
                }
            }
            SelectionPhase::RestoreOutput => {
                let selection_error = state
                    .audio_output
                    .selection_error
                    .clone()
                    .unwrap_or_else(|| "mpv rejected the audio device".to_owned());
                let result = if succeeded {
                    Err(selection_error)
                } else {
                    Err(sanitize_audio_error_text(format!(
                        "{selection_error}; mpv could not restore the forced audio output ({error})"
                    )))
                };
                finish_selection(state);
                emit_selection_result(emit, &selection, result);
            }
        },
    }
    true
}

pub(super) fn fail_pending_commands(state: &DispatchState, emit: &EventSink, error: &str) {
    for pending in state.pending.values() {
        if let Some(acknowledgement) = &pending.acknowledgement {
            acknowledgement.fail(error.to_owned());
        }
        match pending.audio_output.as_ref() {
            Some(Pending::Selection(selection)) => {
                emit_selection_result(emit, selection, Err(sanitize_audio_error_text(error)))
            }
            Some(Pending::Refresh) => emit(PlayerEvent::AudioDeviceRefreshFailed(
                sanitize_audio_error_text(error),
            )),
            None => {}
        }
    }
}

pub(super) fn is_protected(pending: &PendingCommand) -> bool {
    pending.audio_output.is_some()
}

pub(super) fn evict_oldest_unprotected_pending(state: &mut DispatchState) -> bool {
    let oldest = state
        .pending
        .iter()
        .filter(|(_, pending)| {
            pending.file_generation.is_none()
                && pending.acknowledgement.is_none()
                && !is_protected(pending)
        })
        .map(|(request_id, _)| *request_id)
        .min();
    if let Some(oldest) = oldest {
        state.pending.remove(&oldest);
        true
    } else {
        false
    }
}

pub(super) fn remember_refresh(state: &mut DispatchState, request_id: u64) -> bool {
    if state.pending.len() >= 128 && !evict_oldest_unprotected_pending(state) {
        return false;
    }
    state.pending.insert(
        request_id,
        PendingCommand {
            label: "refresh audio-device-list".to_owned(),
            file_generation: None,
            acknowledgement: None,
            audio_output: Some(Pending::Refresh),
        },
    );
    true
}

pub(super) fn remember_selection(
    state: &mut DispatchState,
    request_id: u64,
    selection: Selection,
) -> Result<(), Selection> {
    if state.pending.len() >= 128 && !evict_oldest_unprotected_pending(state) {
        return Err(selection);
    }
    let label = match selection.phase {
        SelectionPhase::InspectOutput => "inspect forced audio output",
        SelectionPhase::ClearOutput => "clear forced audio output",
        SelectionPhase::SetDevice => "select audio device",
        SelectionPhase::RestoreOutput => "restore forced audio output",
    };
    state.pending.insert(
        request_id,
        PendingCommand {
            label: label.to_owned(),
            file_generation: None,
            acknowledgement: None,
            audio_output: Some(Pending::Selection(selection)),
        },
    );
    Ok(())
}

pub(super) fn timeout(state: &mut DispatchState, emit: &EventSink) {
    let Some(request_id) = state.audio_output.selection_request_id else {
        return;
    };
    let selection = state
        .pending
        .remove(&request_id)
        .and_then(|pending| pending.audio_output)
        .and_then(|pending| match pending {
            Pending::Selection(selection) => Some(selection),
            Pending::Refresh => None,
        });
    finish_selection(state);
    if let Some(selection) = selection {
        emit_selection_result(
            emit,
            &selection,
            Err("mpv audio device selection timed out".to_owned()),
        );
    }
}

async fn dispatch_refresh(
    conn: &Stream,
    emit: &EventSink,
    state: &mut DispatchState,
    command_request_id: u64,
) -> io::Result<()> {
    if !remember_refresh(state, command_request_id) {
        emit(PlayerEvent::AudioDeviceRefreshFailed(
            "mpv acknowledgement queue saturated".to_owned(),
        ));
        return Ok(());
    }
    write_json(
        conn,
        &proto::cmd_get_property("audio-device-list", command_request_id),
    )
    .await
}

async fn dispatch_selection(
    conn: &Stream,
    emit: &EventSink,
    state: &mut DispatchState,
    command_request_id: u64,
    correlation_id: u64,
    device: Option<String>,
) -> io::Result<()> {
    let device = match normalize_audio_device_request(device) {
        Ok(device) => device,
        Err(error) => {
            emit(PlayerEvent::AudioDeviceSelectionResult {
                correlation_id,
                device: None,
                result: Err(error.to_owned()),
            });
            return Ok(());
        }
    };
    let selection = Selection {
        correlation_id,
        device,
        phase: SelectionPhase::InspectOutput,
    };
    let json = selection_command(state, &selection, command_request_id);
    if let Err(selection) = remember_selection(state, command_request_id, selection) {
        emit_selection_result(
            emit,
            &selection,
            Err("mpv acknowledgement queue saturated".to_owned()),
        );
        return Ok(());
    }
    state.audio_output.selection_request_id = Some(command_request_id);
    state.audio_output.selection_deadline = Some(Instant::now() + SELECTION_TIMEOUT);
    write_json(conn, &json).await
}

pub(super) fn selection_command(
    state: &DispatchState,
    selection: &Selection,
    request_id: u64,
) -> String {
    match selection.phase {
        SelectionPhase::InspectOutput => proto::cmd_get_property("options/ao", request_id),
        SelectionPhase::ClearOutput => {
            proto::cmd_set_property("options/ao", &Value::from(""), request_id)
        }
        SelectionPhase::SetDevice => proto::cmd_set_property(
            "audio-device",
            &Value::from(selection.device.as_deref().unwrap_or("auto")),
            request_id,
        ),
        SelectionPhase::RestoreOutput => proto::cmd_set_property(
            "options/ao",
            state
                .audio_output
                .prior_output
                .as_ref()
                .expect("rollback is scheduled only for a forced output"),
            request_id,
        ),
    }
}

fn forced_output_value(data: Option<&Value>) -> Option<Value> {
    match data {
        None | Some(Value::Null) => None,
        Some(Value::String(value)) if value.trim().is_empty() => None,
        Some(Value::Array(value)) if value.is_empty() => None,
        Some(value) => Some(value.clone()),
    }
}

fn emit_selection_result(emit: &EventSink, selection: &Selection, result: Result<(), String>) {
    emit(PlayerEvent::AudioDeviceSelectionResult {
        correlation_id: selection.correlation_id,
        device: selection.device.clone(),
        result,
    });
}

fn finish_selection(state: &mut DispatchState) {
    state.audio_output.selection_request_id = None;
    state.audio_output.selection_deadline = None;
    state.audio_output.followup = None;
    state.audio_output.prior_output = None;
    state.audio_output.selection_error = None;
}
