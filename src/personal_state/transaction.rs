#[cfg(unix)]
use std::fs::File;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{PersonalProjection, PersonalStateError, PersonalStateV2, project};

const LEDGER_MAX_BYTES: u64 = crate::data_export::EXPORT_MAX_BYTES;
const TRANSACTION_MANIFEST_MAX_BYTES: u64 = 128 * 1024;
const TRANSACTION_SCAN_MAX: usize = 64;
const TRANSACTION_DIR: &str = ".personal-state-v2-transactions";
const STATE_DIR: &str = "personal_state_v2";
const LEDGER_FILE: &str = "ledger.json";
const MANIFEST_FILE: &str = "manifest.json";
const COMMIT_MARKER: &str = "committed";
const COMPLETE_MARKER: &str = "complete";

#[derive(Debug, Clone)]
pub struct PersonalStatePaths {
    pub data_root: PathBuf,
    pub state_dir: PathBuf,
    pub ledger: PathBuf,
    pub transactions: PathBuf,
    pub library: PathBuf,
    pub signals: PathBuf,
    pub playlists: PathBuf,
    pub station: PathBuf,
}

impl PersonalStatePaths {
    pub fn for_data_root(data_root: PathBuf) -> Self {
        Self {
            state_dir: data_root.join(STATE_DIR),
            ledger: data_root.join(STATE_DIR).join(LEDGER_FILE),
            transactions: data_root.join(TRANSACTION_DIR),
            library: data_root.join("library.json"),
            signals: data_root.join("signals.json"),
            playlists: data_root.join("playlists.json"),
            station: data_root.join("station.json"),
            data_root,
        }
    }

    pub fn current() -> Result<Self, PersonalStateError> {
        crate::paths::data_dir()
            .map(Self::for_data_root)
            .ok_or_else(|| PersonalStateError::Io("data directory is unavailable".to_owned()))
    }
}

#[derive(Debug, Clone)]
pub struct PersonalStateCommit {
    state: PersonalStateV2,
    projection: PersonalProjection,
    library: crate::library::Library,
    playlists: crate::playlists::Playlists,
    signals: crate::signals::Signals,
    station: crate::station::StationStore,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TransactionManifest {
    schema_version: u32,
    transaction_id: String,
    revision: u64,
    projection_fingerprint: String,
    files: Vec<TransactionFile>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TransactionFile {
    target: TargetFile,
    staged_name: String,
    sha256: String,
    bytes: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum TargetFile {
    Ledger,
    Library,
    Signals,
    Playlists,
    Station,
}

impl PersonalStateCommit {
    pub fn prepare(state: PersonalStateV2) -> Result<Self, PersonalStateError> {
        Self::prepare_with_playlist_revision(state, 0)
    }

    pub(crate) fn prepare_for_runtime(
        state: PersonalStateV2,
        playlist_revision: u64,
    ) -> Result<Self, PersonalStateError> {
        Self::prepare_with_playlist_revision(state, playlist_revision)
    }

    fn prepare_with_playlist_revision(
        mut state: PersonalStateV2,
        playlist_revision: u64,
    ) -> Result<Self, PersonalStateError> {
        state.validate()?;
        let projection = project(&state)?;
        if state.projection_fingerprint.as_deref() != Some(projection.fingerprint.as_str()) {
            state.revision = state.revision.saturating_add(1);
        }
        state.device_registry = projection.device_registry.clone();
        state.version_vector = projection.version_vector.clone();
        state.projection_fingerprint = Some(projection.fingerprint.clone());
        let (library, mut playlists, signals, station) = projection.clone().into_runtime();
        playlists.revision = playlist_revision;
        Ok(Self {
            state,
            projection,
            library,
            playlists,
            signals,
            station,
        })
    }

    pub fn state(&self) -> &PersonalStateV2 {
        &self.state
    }

    pub fn projection(&self) -> &PersonalProjection {
        &self.projection
    }

    pub(crate) fn runtime_stores(
        &self,
    ) -> (
        crate::library::Library,
        crate::playlists::Playlists,
        crate::signals::Signals,
        crate::station::StationStore,
    ) {
        (
            self.library.clone(),
            self.playlists.clone(),
            self.signals.clone(),
            self.station.clone(),
        )
    }

    pub fn commit(
        &self,
        paths: &PersonalStatePaths,
    ) -> Result<PersonalStateV2, PersonalStateError> {
        self.commit_using(paths, |_| Ok(()))
    }

    fn commit_using(
        &self,
        paths: &PersonalStatePaths,
        mut checkpoint: impl FnMut(CommitPoint) -> io::Result<()>,
    ) -> Result<PersonalStateV2, PersonalStateError> {
        crate::persist::ensure_persistence_writes_allowed()?;
        ensure_real_directory(&paths.data_root)?;
        ensure_private_directory(&paths.state_dir)?;
        ensure_private_directory(&paths.transactions)?;
        crate::persist::with_store_intent_lock(&paths.ledger, || {
            Ok(self.commit_locked(paths, &mut checkpoint))
        })?
    }

    fn commit_locked(
        &self,
        paths: &PersonalStatePaths,
        checkpoint: &mut impl FnMut(CommitPoint) -> io::Result<()>,
    ) -> Result<PersonalStateV2, PersonalStateError> {
        recover_pending_transactions(paths)?;
        if let Some(installed) = read_installed_ledger(paths)? {
            if installed.revision > self.state.revision {
                return Ok(installed);
            }
            if installed.revision == self.state.revision {
                if installed == self.state {
                    return Ok(installed);
                }
                return Err(PersonalStateError::Io(
                    "personal-state revision collision refused".to_owned(),
                ));
            }
        }

        let transaction_id = random_id("txn")?;
        let transaction_dir = paths.transactions.join(&transaction_id);
        ensure_private_directory_new(&transaction_dir)?;

        let payloads = [
            (TargetFile::Ledger, "ledger.json", pretty_json(&self.state)?),
            (
                TargetFile::Library,
                "library.json",
                pretty_json(&self.library)?,
            ),
            (
                TargetFile::Signals,
                "signals.json",
                pretty_json(&self.signals)?,
            ),
            (
                TargetFile::Playlists,
                "playlists.json",
                pretty_json(&self.playlists)?,
            ),
            (
                TargetFile::Station,
                "station.json",
                pretty_json(&self.station)?,
            ),
        ];
        let mut files = Vec::with_capacity(payloads.len());
        for (target, staged_name, bytes) in &payloads {
            let staged_path = transaction_dir.join(staged_name);
            write_new_private_synced(&staged_path, bytes)?;
            files.push(TransactionFile {
                target: *target,
                staged_name: (*staged_name).to_owned(),
                sha256: sha256_hex(bytes),
                bytes: bytes.len() as u64,
            });
        }
        sync_directory(&transaction_dir)?;
        checkpoint(CommitPoint::Staged)?;

        let manifest = TransactionManifest {
            schema_version: 1,
            transaction_id: transaction_id.clone(),
            revision: self.state.revision,
            projection_fingerprint: self.projection.fingerprint.clone(),
            files,
        };
        let manifest_bytes = pretty_json(&manifest)?;
        write_new_private_synced(&transaction_dir.join(MANIFEST_FILE), &manifest_bytes)?;
        sync_directory(&transaction_dir)?;
        checkpoint(CommitPoint::Manifest)?;

        write_new_private_synced(&transaction_dir.join(COMMIT_MARKER), b"committed\n")?;
        sync_directory(&transaction_dir)?;
        sync_directory(&paths.transactions)?;
        checkpoint(CommitPoint::Committed)?;

        install_transaction(paths, &transaction_dir, &manifest, |target| {
            checkpoint(CommitPoint::Installed(target))
        })?;

        write_new_private_synced(&transaction_dir.join(COMPLETE_MARKER), b"complete\n")?;
        sync_directory(&transaction_dir)?;
        sync_directory(&paths.transactions)?;
        checkpoint(CommitPoint::Completed)?;
        remove_completed_transaction(paths, &transaction_dir)?;
        Ok(self.state.clone())
    }

    #[cfg(test)]
    pub(crate) fn commit_with_failure_at(
        &self,
        paths: &PersonalStatePaths,
        fail_at: CommitPoint,
    ) -> Result<PersonalStateV2, PersonalStateError> {
        self.commit_using(paths, |point| {
            if point == fail_at {
                Err(io::Error::other(format!("injected crash at {point:?}")))
            } else {
                Ok(())
            }
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CommitPoint {
    Staged,
    Manifest,
    Committed,
    Installed(TargetFile),
    Completed,
}

pub fn recover_pending_transactions(paths: &PersonalStatePaths) -> Result<(), PersonalStateError> {
    let entries = match fs::read_dir(&paths.transactions) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    let mut transaction_dirs = Vec::new();
    for entry in entries {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            return Err(PersonalStateError::Io(
                "unexpected file in personal-state transaction directory".to_owned(),
            ));
        }
        transaction_dirs.push(entry.path());
        if transaction_dirs.len() > TRANSACTION_SCAN_MAX {
            return Err(PersonalStateError::Io(
                "too many pending personal-state transactions".to_owned(),
            ));
        }
    }
    transaction_dirs.sort();
    for transaction_dir in transaction_dirs {
        let committed = regular_file_exists(&transaction_dir.join(COMMIT_MARKER))?;
        if !committed {
            discard_uncommitted_transaction(paths, &transaction_dir)?;
            continue;
        }
        let manifest_bytes = crate::util::safe_fs::read_no_symlink_limited(
            &transaction_dir.join(MANIFEST_FILE),
            TRANSACTION_MANIFEST_MAX_BYTES,
        )?;
        let manifest: TransactionManifest = serde_json::from_slice(&manifest_bytes)?;
        validate_manifest(&transaction_dir, &manifest)?;
        install_transaction(paths, &transaction_dir, &manifest, |_| Ok(()))?;
        if !regular_file_exists(&transaction_dir.join(COMPLETE_MARKER))? {
            write_new_private_synced(&transaction_dir.join(COMPLETE_MARKER), b"complete\n")?;
            sync_directory(&transaction_dir)?;
        }
        remove_completed_transaction(paths, &transaction_dir)?;
    }
    Ok(())
}

pub(crate) fn load_ledger(
    paths: &PersonalStatePaths,
) -> Result<Option<PersonalStateV2>, PersonalStateError> {
    recover_pending_transactions(paths)?;
    read_installed_ledger(paths)
}

fn read_installed_ledger(
    paths: &PersonalStatePaths,
) -> Result<Option<PersonalStateV2>, PersonalStateError> {
    let bytes = match crate::util::safe_fs::read_no_symlink_limited(&paths.ledger, LEDGER_MAX_BYTES)
    {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let mut state: PersonalStateV2 = serde_json::from_slice(&bytes)?;
    state.normalize()?;
    let projection = project(&state)?;
    if state.projection_fingerprint.as_deref() != Some(projection.fingerprint.as_str()) {
        return Err(PersonalStateError::ProjectionMismatch);
    }
    Ok(Some(state))
}

fn install_transaction(
    paths: &PersonalStatePaths,
    transaction_dir: &Path,
    manifest: &TransactionManifest,
    mut after_install: impl FnMut(TargetFile) -> io::Result<()>,
) -> Result<(), PersonalStateError> {
    for file in &manifest.files {
        let staged_path = transaction_dir.join(&file.staged_name);
        let staged =
            crate::util::safe_fs::read_no_symlink_limited(&staged_path, max_for(file.target))?;
        if staged.len() as u64 != file.bytes || sha256_hex(&staged) != file.sha256 {
            return Err(PersonalStateError::Io(
                "personal-state transaction payload hash mismatch".to_owned(),
            ));
        }
        let target_path = target_path(paths, file.target);
        let installed = match crate::util::safe_fs::read_no_symlink_limited(
            target_path,
            max_for(file.target),
        ) {
            Ok(bytes) => bytes.len() as u64 == file.bytes && sha256_hex(&bytes) == file.sha256,
            Err(error) if error.kind() == io::ErrorKind::NotFound => false,
            Err(error) => return Err(error.into()),
        };
        if !installed {
            if let Some(parent) = target_path.parent() {
                if file.target == TargetFile::Ledger {
                    ensure_private_directory(parent)?;
                } else {
                    ensure_real_directory(parent)?;
                }
            }
            crate::util::safe_fs::write_private_atomic(target_path, &staged)?;
            if let Some(parent) = target_path.parent() {
                sync_directory(parent)?;
            }
        }
        after_install(file.target)?;
    }
    Ok(())
}

fn validate_manifest(
    transaction_dir: &Path,
    manifest: &TransactionManifest,
) -> Result<(), PersonalStateError> {
    if manifest.schema_version != 1
        || transaction_dir.file_name().and_then(|name| name.to_str())
            != Some(manifest.transaction_id.as_str())
        || manifest.files.len() != 5
    {
        return Err(PersonalStateError::Io(
            "invalid personal-state transaction manifest".to_owned(),
        ));
    }
    let mut targets = manifest
        .files
        .iter()
        .map(|file| file.target)
        .collect::<Vec<_>>();
    targets.sort_by_key(|target| *target as u8);
    targets.dedup();
    if targets.len() != 5 {
        return Err(PersonalStateError::Io(
            "personal-state transaction manifest is incomplete".to_owned(),
        ));
    }
    for file in &manifest.files {
        if file.staged_name != staged_name(file.target)
            || file.bytes == 0
            || file.bytes > max_for(file.target)
            || file.sha256.len() != 64
            || !file.sha256.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(PersonalStateError::Io(
                "invalid personal-state transaction file record".to_owned(),
            ));
        }
    }
    Ok(())
}

fn target_path(paths: &PersonalStatePaths, target: TargetFile) -> &Path {
    match target {
        TargetFile::Ledger => &paths.ledger,
        TargetFile::Library => &paths.library,
        TargetFile::Signals => &paths.signals,
        TargetFile::Playlists => &paths.playlists,
        TargetFile::Station => &paths.station,
    }
}

fn staged_name(target: TargetFile) -> &'static str {
    match target {
        TargetFile::Ledger => "ledger.json",
        TargetFile::Library => "library.json",
        TargetFile::Signals => "signals.json",
        TargetFile::Playlists => "playlists.json",
        TargetFile::Station => "station.json",
    }
}

fn max_for(target: TargetFile) -> u64 {
    match target {
        TargetFile::Ledger => LEDGER_MAX_BYTES,
        TargetFile::Library | TargetFile::Playlists => 50 * 1024 * 1024,
        TargetFile::Signals => 32 * 1024 * 1024,
        TargetFile::Station => 16 * 1024 * 1024,
    }
}

fn pretty_json(value: &impl Serialize) -> Result<Vec<u8>, PersonalStateError> {
    let mut bytes = serde_json::to_vec_pretty(value)?;
    bytes.push(b'\n');
    Ok(bytes)
}

fn write_new_private_synced(path: &Path, bytes: &[u8]) -> io::Result<()> {
    #[cfg(unix)]
    use std::os::unix::fs::OpenOptionsExt as _;

    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options.open(path)?;
    file.write_all(bytes)?;
    file.flush()?;
    file.sync_all()
}

fn ensure_private_directory(path: &Path) -> io::Result<()> {
    ensure_real_directory(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn ensure_real_directory(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "personal-state directory is not a real directory",
            ));
        }
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            fs::create_dir_all(path)?;
        }
        Err(error) => return Err(error),
    }
    Ok(())
}

fn ensure_private_directory_new(path: &Path) -> io::Result<()> {
    fs::create_dir(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> io::Result<()> {
    File::open(path)?.sync_all()
}

#[cfg(not(unix))]
fn sync_directory(_path: &Path) -> io::Result<()> {
    Ok(())
}

fn regular_file_exists(path: &Path) -> io::Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "personal-state marker is not a regular file",
            ))
        }
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

fn discard_uncommitted_transaction(
    paths: &PersonalStatePaths,
    transaction_dir: &Path,
) -> io::Result<()> {
    fs::remove_dir_all(transaction_dir)?;
    sync_directory(&paths.transactions)
}

fn remove_completed_transaction(
    paths: &PersonalStatePaths,
    transaction_dir: &Path,
) -> io::Result<()> {
    fs::remove_dir_all(transaction_dir)?;
    sync_directory(&paths.transactions)
}

fn random_id(prefix: &str) -> Result<String, PersonalStateError> {
    let mut random = [0_u8; 16];
    getrandom::fill(&mut random)
        .map_err(|error| PersonalStateError::Io(format!("randomness unavailable: {error}")))?;
    let mut suffix = String::with_capacity(random.len() * 2);
    use std::fmt::Write as _;
    for byte in random {
        let _ = write!(suffix, "{byte:02x}");
    }
    Ok(format!("{prefix}-{suffix}"))
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(digest.len() * 2);
    use std::fmt::Write as _;
    for byte in digest {
        let _ = write!(out, "{byte:02x}");
    }
    out
}
