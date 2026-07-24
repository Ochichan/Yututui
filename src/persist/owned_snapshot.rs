use super::*;

/// Immutable snapshot taken at send time so the actor never reaches back into `App`.
pub enum Snapshot {
    PersonalState(Box<crate::personal_state::PersonalStateCommit>),
    /// A WebDAV result whose ledger and private checkpoint anchor commit together.
    PersonalSync(crate::sync::service::PersonalSyncPersistence),
    Library(Arc<crate::library::Library>),
    Signals(Arc<crate::signals::Signals>),
    Downloads(crate::downloads::DownloadStore),
    Config(Box<crate::config::Config>),
    Playlists(Arc<crate::playlists::Playlists>),
    Station(crate::station::StationStore),
    RomanizedTitles(crate::romanize::RomanizeCache),
    Session(crate::session::SessionCache),
    #[cfg(test)]
    Test {
        kind: StoreKind,
        label: &'static str,
        storage_path: Option<PathBuf>,
        writer: Arc<dyn Fn() -> std::io::Result<()> + Send + Sync>,
    },
}

#[cfg(test)]
impl Snapshot {
    pub(super) fn kind(&self) -> StoreKind {
        match self {
            Self::PersonalState(_) | Self::PersonalSync(_) => StoreKind::PersonalState,
            Self::Library(_) => StoreKind::Library,
            Self::Signals(_) => StoreKind::Signals,
            Self::Downloads(_) => StoreKind::Downloads,
            Self::Config(_) => StoreKind::Config,
            Self::Playlists(_) => StoreKind::Playlists,
            Self::Station(_) => StoreKind::Station,
            Self::RomanizedTitles(_) => StoreKind::RomanizedTitles,
            Self::Session(_) => StoreKind::Session,
            Self::Test { kind, .. } => *kind,
        }
    }
}

/// Private thread-shareable form of the public admission API.
///
/// `RomanizeCache` carries a render-only `RefCell` scratch buffer, so moving that public payload
/// directly behind `Arc` would make every snapshot non-`Sync`. Admission moves only its serialized
/// `entries` state into a DTO; every other variant is moved unchanged without cloning.
pub(super) enum OwnedSnapshot {
    PersonalState(Box<crate::personal_state::PersonalStateCommit>),
    PersonalSync(crate::sync::service::PersonalSyncPersistence),
    Library(Arc<crate::library::Library>),
    Signals(Arc<crate::signals::Signals>),
    Downloads(crate::downloads::DownloadStore),
    Config(Box<crate::config::Config>),
    Playlists(Arc<crate::playlists::Playlists>),
    Station(crate::station::StationStore),
    RomanizedTitles(crate::romanize::RomanizePersistSnapshot),
    Session(crate::session::SessionCache),
    #[cfg(test)]
    Test {
        kind: StoreKind,
        label: &'static str,
        storage_path: Option<PathBuf>,
        writer: Arc<dyn Fn() -> std::io::Result<()> + Send + Sync>,
    },
}

impl From<Snapshot> for OwnedSnapshot {
    fn from(snapshot: Snapshot) -> Self {
        match snapshot {
            Snapshot::PersonalState(value) => Self::PersonalState(value),
            Snapshot::PersonalSync(value) => Self::PersonalSync(value),
            Snapshot::Library(value) => Self::Library(value),
            Snapshot::Signals(value) => Self::Signals(value),
            Snapshot::Downloads(value) => Self::Downloads(value),
            Snapshot::Config(value) => Self::Config(value),
            Snapshot::Playlists(value) => Self::Playlists(value),
            Snapshot::Station(value) => Self::Station(value),
            Snapshot::RomanizedTitles(value) => {
                Self::RomanizedTitles(value.into_persist_snapshot())
            }
            Snapshot::Session(value) => Self::Session(value),
            #[cfg(test)]
            Snapshot::Test {
                kind,
                label,
                storage_path,
                writer,
            } => Self::Test {
                kind,
                label,
                storage_path,
                writer,
            },
        }
    }
}

impl OwnedSnapshot {
    pub(super) fn kind(&self) -> StoreKind {
        match self {
            Self::PersonalState(_) => StoreKind::PersonalState,
            Self::PersonalSync(_) => StoreKind::PersonalState,
            Self::Library(_) => StoreKind::Library,
            Self::Signals(_) => StoreKind::Signals,
            Self::Downloads(_) => StoreKind::Downloads,
            Self::Config(_) => StoreKind::Config,
            Self::Playlists(_) => StoreKind::Playlists,
            Self::Station(_) => StoreKind::Station,
            Self::RomanizedTitles(_) => StoreKind::RomanizedTitles,
            Self::Session(_) => StoreKind::Session,
            #[cfg(test)]
            Self::Test { kind, .. } => *kind,
        }
    }

    pub(super) fn write(&self) -> std::io::Result<()> {
        match self {
            Self::PersonalState(value) => {
                let paths = crate::personal_state::PersonalStatePaths::current()
                    .map_err(std::io::Error::other)?;
                value
                    .commit(&paths)
                    .map(|_| ())
                    .map_err(std::io::Error::other)
            }
            Self::PersonalSync(value) => value.write().map_err(std::io::Error::other),
            Self::Library(value) => value.as_ref().save(),
            Self::Signals(value) => value.as_ref().save(),
            Self::Downloads(value) => value.save(),
            Self::Config(value) => match crate::config::config_path() {
                Some(path) => crate::persist::write_store_json(&path, value.as_ref()),
                None => Ok(()),
            },
            Self::Playlists(value) => value.as_ref().save(),
            Self::Station(value) => value.save(),
            Self::RomanizedTitles(value) => value.save(),
            Self::Session(value) => value.save(),
            #[cfg(test)]
            Self::Test { writer, .. } => writer(),
        }
    }

    pub(super) fn storage_path(&self) -> Option<PathBuf> {
        match self {
            Self::PersonalState(_) => None,
            Self::PersonalSync(_) => None,
            Self::Library(_) => crate::library::library_path(),
            Self::Signals(_) => crate::signals::signals_path(),
            Self::Downloads(_) => crate::downloads::store_path(),
            Self::Config(_) => crate::config::config_path(),
            Self::Playlists(_) => crate::playlists::playlists_path(),
            Self::Station(_) => crate::station::station_path(),
            Self::RomanizedTitles(_) => crate::romanize::cache_path(),
            Self::Session(_) => crate::session::session_cache_path(),
            #[cfg(test)]
            Self::Test { storage_path, .. } => storage_path.clone(),
        }
    }

    pub(super) fn to_json_bytes(&self) -> serde_json::Result<Vec<u8>> {
        match self {
            Self::PersonalState(value) => serde_json::to_vec_pretty(value.state()),
            Self::PersonalSync(value) => serde_json::to_vec_pretty(value.state()),
            Self::Library(value) => serde_json::to_vec_pretty(value.as_ref()),
            Self::Signals(value) => serde_json::to_vec_pretty(value.as_ref()),
            Self::Downloads(value) => serde_json::to_vec_pretty(value),
            Self::Config(value) => serde_json::to_vec_pretty(value),
            Self::Playlists(value) => serde_json::to_vec_pretty(value.as_ref()),
            Self::Station(value) => serde_json::to_vec_pretty(value),
            Self::RomanizedTitles(value) => serde_json::to_vec_pretty(value),
            Self::Session(value) => serde_json::to_vec_pretty(value),
            #[cfg(test)]
            Self::Test { .. } => serde_json::to_vec(&serde_json::Value::Null),
        }
    }

    pub(super) fn label(&self) -> &'static str {
        #[cfg(test)]
        if let Self::Test { label, .. } = self {
            return label;
        }
        if matches!(self, Self::PersonalSync(_)) {
            return "personal sync";
        }
        self.kind().label()
    }
}
