//! Bounded, preview-first import of portable personal-state exports.

use std::fmt;
use std::io;
use std::path::Path;

use crate::personal_state::{
    ImportPlan, PersonalStateCommit, PersonalStateError, PersonalStatePaths, PersonalStateV2,
    legacy_state, plan_import,
};

#[derive(Debug)]
pub enum ImportError {
    InvalidSource(&'static str),
    Io(io::Error),
    State(PersonalStateError),
    LegacySource(String),
}

impl fmt::Display for ImportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidSource(reason) => write!(f, "invalid import file: {reason}"),
            Self::Io(error) => write!(f, "could not read import file: {error}"),
            Self::State(error) => write!(f, "could not import personal state: {error}"),
            Self::LegacySource(error) => {
                write!(f, "could not load the current personal state: {error}")
            }
        }
    }
}

impl std::error::Error for ImportError {}

impl From<io::Error> for ImportError {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}

impl From<PersonalStateError> for ImportError {
    fn from(value: PersonalStateError) -> Self {
        Self::State(value)
    }
}

pub fn plan_from_file(path: &Path) -> Result<(PersonalStatePaths, ImportPlan), ImportError> {
    plan_from_file_with_recovery(path, true)
}

/// Preview an import from the currently installed coherent frontier without acquiring a writer
/// lease or repairing transaction artifacts.
pub fn preview_from_file(path: &Path) -> Result<(PersonalStatePaths, ImportPlan), ImportError> {
    plan_from_file_with_recovery(path, false)
}

fn plan_from_file_with_recovery(
    path: &Path,
    recover_pending: bool,
) -> Result<(PersonalStatePaths, ImportPlan), ImportError> {
    let bytes =
        crate::util::safe_fs::read_no_symlink_limited(path, crate::data_export::EXPORT_MAX_BYTES)
            .map_err(|error| {
            if error.kind() == io::ErrorKind::InvalidData {
                ImportError::InvalidSource("file exceeds the 192 MiB safety limit")
            } else {
                ImportError::Io(error)
            }
        })?;
    if bytes.is_empty() {
        return Err(ImportError::InvalidSource("file is empty"));
    }
    let imported = crate::data_export::decode_personal_state_export(&bytes)?;
    let paths = PersonalStatePaths::current()?;
    let current = load_current_state(&paths, recover_pending)?;
    let plan = plan_import(&current, &imported)?;
    Ok((paths, plan))
}

pub fn apply_plan(
    paths: &PersonalStatePaths,
    plan: ImportPlan,
) -> Result<PersonalStateV2, ImportError> {
    if !plan.summary.changed {
        return Ok(plan.candidate);
    }
    let commit = PersonalStateCommit::prepare(plan.candidate)?;
    commit.commit(paths).map_err(ImportError::from)
}

fn load_current_state(
    paths: &PersonalStatePaths,
    recover_pending: bool,
) -> Result<PersonalStateV2, ImportError> {
    let loaded = if recover_pending {
        crate::personal_state::load_ledger(paths)
    } else {
        crate::personal_state::load_ledger_read_only(paths)
    }?;
    let sources = crate::data_export::offline::load_sources()
        .map_err(|error| ImportError::LegacySource(error.to_string()))?;
    if !recover_pending {
        let verified = crate::personal_state::load_ledger_read_only(paths)?;
        if verified != loaded {
            return Err(PersonalStateError::Io(
                "personal state changed while building the import preview; retry".to_owned(),
            )
            .into());
        }
    }
    match loaded {
        Some(state) => {
            let local_device = crate::persist::load_personal_state_device_id(&state)
                .map_err(|error| ImportError::LegacySource(error.to_string()))?;
            crate::data_export::reconcile_v2_sources(
                &state,
                local_device.as_ref(),
                &sources.library,
                &sources.playlists,
                &sources.signals,
                &sources.station,
            )
        }
        None => legacy_state(
            &sources.library,
            &sources.playlists,
            &sources.signals,
            &sources.station,
        ),
    }
    .map_err(ImportError::from)
}
