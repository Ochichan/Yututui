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
    let current = load_current_state(&paths)?;
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

fn load_current_state(paths: &PersonalStatePaths) -> Result<PersonalStateV2, ImportError> {
    let sources = crate::data_export::offline::load_sources()
        .map_err(|error| ImportError::LegacySource(error.to_string()))?;
    match crate::personal_state::load_ledger(paths)? {
        Some(state) => crate::personal_state::reconcile_runtime(
            &state,
            &sources.library,
            &sources.playlists,
            &sources.signals,
            &sources.station,
        ),
        None => legacy_state(
            &sources.library,
            &sources.playlists,
            &sources.signals,
            &sources.station,
        ),
    }
    .map_err(ImportError::from)
}
