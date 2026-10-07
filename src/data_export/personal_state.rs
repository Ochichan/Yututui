use std::path::{Path, PathBuf};

use crate::library::Library;
use crate::playlists::Playlists;
use crate::signals::Signals;
use crate::station::StationStore;

use super::{
    ExportError, ExportSchema, FILE_PREFIX_V2, FILE_PREFIX_V3, export_serializable, unix_now,
};

pub fn export_personal_state_snapshot(
    directory: &Path,
    state: &crate::personal_state::PersonalStateV2,
) -> Result<PathBuf, ExportError> {
    state.validate().map_err(|error| ExportError::SourceStore {
        store: "personal state",
        detail: error.to_string(),
    })?;
    let prefix =
        if state.schema_version == crate::personal_state::PERSONAL_STATE_LISTENING_SCHEMA_VERSION {
            FILE_PREFIX_V3
        } else {
            FILE_PREFIX_V2
        };
    export_serializable(directory, state, unix_now(), prefix)
}

#[allow(clippy::too_many_arguments)]
pub fn export_v2_from_sources(
    directory: &Path,
    personal_state: &crate::personal_state::PersonalStateV2,
    local_device: Option<&crate::personal_state::DeviceId>,
    library: &Library,
    playlists: &Playlists,
    signals: &Signals,
    station: &StationStore,
) -> Result<PathBuf, ExportError> {
    export_personal_state_from_sources(
        directory,
        ExportSchema::V2,
        personal_state,
        local_device,
        library,
        playlists,
        signals,
        station,
    )
}

#[allow(clippy::too_many_arguments)]
pub fn export_personal_state_from_sources(
    directory: &Path,
    schema: ExportSchema,
    personal_state: &crate::personal_state::PersonalStateV2,
    local_device: Option<&crate::personal_state::DeviceId>,
    library: &Library,
    playlists: &Playlists,
    signals: &Signals,
    station: &StationStore,
) -> Result<PathBuf, ExportError> {
    if schema == ExportSchema::V1 {
        return Err(ExportError::SourceStore {
            store: "personal state",
            detail: "schema 1 requires the legacy portable snapshot exporter".to_owned(),
        });
    }
    let mut state = reconcile_v2_sources(
        personal_state,
        local_device,
        library,
        playlists,
        signals,
        station,
    )
    .and_then(crate::personal_state::PersonalStateCommit::prepare)
    .map_err(|error| ExportError::SourceStore {
        store: "personal state",
        detail: error.to_string(),
    })?;
    if schema == ExportSchema::V2
        && state.state().schema_version != crate::personal_state::PERSONAL_STATE_SCHEMA_VERSION
    {
        return Err(ExportError::SourceStore {
            store: "personal state",
            detail: "schema 2 cannot represent listening records; export schema 3 instead"
                .to_owned(),
        });
    }
    if schema == ExportSchema::V3
        && state.state().schema_version == crate::personal_state::PERSONAL_STATE_SCHEMA_VERSION
    {
        let mut upgraded = state.state().clone();
        upgraded.schema_version = crate::personal_state::PERSONAL_STATE_LISTENING_SCHEMA_VERSION;
        state = crate::personal_state::PersonalStateCommit::prepare(upgraded).map_err(|error| {
            ExportError::SourceStore {
                store: "personal state",
                detail: error.to_string(),
            }
        })?;
    }
    export_personal_state_snapshot(directory, state.state())
}

pub(crate) fn reconcile_v2_sources(
    personal_state: &crate::personal_state::PersonalStateV2,
    local_device: Option<&crate::personal_state::DeviceId>,
    library: &Library,
    playlists: &Playlists,
    signals: &Signals,
    station: &StationStore,
) -> Result<crate::personal_state::PersonalStateV2, crate::personal_state::PersonalStateError> {
    match local_device {
        Some(device_id) => crate::personal_state::reconcile_runtime_as(
            personal_state,
            device_id,
            library,
            playlists,
            signals,
            station,
        ),
        None => crate::personal_state::reconcile_runtime(
            personal_state,
            library,
            playlists,
            signals,
            station,
        ),
    }
}
