//! Metadata-only invalidation for persisted Local Deck import projections.

use std::collections::hash_map::DefaultHasher;
use std::ffi::OsStr;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

const LOCAL_IMPORT_RETAINED_PATH_CAP: usize = 4096;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct LocalImportFilesFingerprint {
    digest: u64,
    reliable: bool,
}

impl LocalImportFilesFingerprint {
    pub(super) fn digest(self) -> u64 {
        self.digest
    }

    pub(super) fn reliable(self) -> bool {
        self.reliable
    }
}

pub(super) fn stable_import_cache_key(
    before: Option<LocalImportFilesFingerprint>,
    after: Option<LocalImportFilesFingerprint>,
) -> Option<Option<u64>> {
    match (before, after) {
        (None, None) => Some(None),
        (Some(before), Some(after))
            if before.reliable && after.reliable && before.digest == after.digest =>
        {
            Some(Some(after.digest))
        }
        _ => None,
    }
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(in crate::app) struct LocalImportFingerprintProbe {
    pub directory_metadata_stats: u64,
    pub directory_generation_queries: u64,
    pub read_dir_calls: u64,
    pub read_dir_errors: u64,
    pub directory_entries_visited: u64,
    pub artifact_metadata_stats: u64,
}

#[derive(Default)]
struct CachedImportDirectory {
    path: Option<PathBuf>,
    metadata: Option<ImportMetadataState>,
    retained_paths: Vec<PathBuf>,
    recognized_count: usize,
    membership_valid: bool,
    overflow: bool,
    #[cfg(windows)]
    directory_change_time: Option<i64>,
    #[cfg(test)]
    next_read_dir_error: Option<std::io::ErrorKind>,
}

/// One LocalMode-owned cache. Directory membership is retained only while it is reliable and
/// bounded; overflow/error states deliberately rescan on every lookup rather than missing writes.
#[derive(Default)]
pub(in crate::app) struct LocalImportFilesFingerprintCache {
    transfers: CachedImportDirectory,
    sessions: CachedImportDirectory,
    force_full_scan: bool,
    #[cfg(test)]
    probe: LocalImportFingerprintProbe,
    #[cfg(test)]
    data_dir_override: Option<PathBuf>,
}

impl LocalImportFilesFingerprintCache {
    #[cfg(test)]
    pub(in crate::app) fn set_data_dir_for_test(&mut self, data_dir: PathBuf) {
        *self = Self {
            data_dir_override: Some(data_dir),
            ..Self::default()
        };
    }

    #[cfg(test)]
    pub(in crate::app) fn reset_probe(&mut self) {
        self.probe = LocalImportFingerprintProbe::default();
    }

    #[cfg(test)]
    pub(in crate::app) fn probe(&self) -> LocalImportFingerprintProbe {
        self.probe
    }

    #[cfg(test)]
    pub(in crate::app) fn fail_next_transfers_read_dir_for_test(
        &mut self,
        kind: std::io::ErrorKind,
    ) {
        self.transfers.membership_valid = false;
        self.transfers.next_read_dir_error = Some(kind);
    }

    #[cfg(test)]
    pub(in crate::app) fn retained_path_count_for_test(&self) -> usize {
        self.transfers.retained_paths.len() + self.sessions.retained_paths.len()
    }

    #[cfg(test)]
    pub(in crate::app) fn retained_path_capacity_for_test(&self) -> usize {
        self.transfers.retained_paths.capacity() + self.sessions.retained_paths.capacity()
    }
}

#[derive(Clone, Copy)]
enum ImportArtifactDirectory {
    Transfers,
    Sessions,
}

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
enum ImportModifiedTimeSignature {
    SinceEpoch { secs: u64, nanos: u32 },
    BeforeEpoch { secs: u64, nanos: u32 },
    Unavailable(std::io::ErrorKind),
}

#[cfg(unix)]
#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
struct ImportPlatformMetadataSignature {
    device: u64,
    inode: u64,
    mode: u32,
    links: u64,
    changed_secs: i64,
    changed_nanos: i64,
}

#[cfg(windows)]
#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
struct ImportPlatformMetadataSignature {
    attributes: u32,
    creation_time: u64,
    last_write_time: u64,
    artifact_change_time: Option<i64>,
}

#[cfg(not(any(unix, windows)))]
#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
struct ImportPlatformMetadataSignature;

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
struct ImportMetadataSignature {
    len: u64,
    is_file: bool,
    is_dir: bool,
    is_symlink: bool,
    readonly: bool,
    modified: ImportModifiedTimeSignature,
    platform: ImportPlatformMetadataSignature,
}

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
enum ImportMetadataState {
    Present(ImportMetadataSignature),
    Error(std::io::ErrorKind),
}

#[derive(Default)]
struct ImportEntryAccumulator {
    count: u64,
    sum: u64,
    xor: u64,
}

impl ImportEntryAccumulator {
    fn push(&mut self, value: u64) {
        self.count = self.count.wrapping_add(1);
        self.sum = self.sum.wrapping_add(value);
        self.xor ^= value.rotate_left((value & 63) as u32);
    }

    fn hash_into(&self, hasher: &mut impl Hasher) {
        self.count.hash(hasher);
        self.sum.hash(hasher);
        self.xor.hash(hasher);
    }
}

pub(super) fn local_import_files_fingerprint(
    cache: &mut LocalImportFilesFingerprintCache,
) -> LocalImportFilesFingerprint {
    #[cfg(test)]
    let data_dir = cache
        .data_dir_override
        .clone()
        .or_else(crate::paths::data_dir);
    #[cfg(not(test))]
    let data_dir = crate::paths::data_dir();
    let Some(data_dir) = data_dir else {
        return LocalImportFilesFingerprint {
            digest: 0,
            reliable: true,
        };
    };
    let transfers = data_dir.join("transfers");
    let sessions = transfers.join("sessions");
    let force_full_scan = cache.force_full_scan;
    let transfers_budget =
        LOCAL_IMPORT_RETAINED_PATH_CAP.saturating_sub(cache.sessions.retained_paths.len());
    let transfers_fingerprint = fingerprint_import_directory(
        &transfers,
        ImportArtifactDirectory::Transfers,
        &mut cache.transfers,
        transfers_budget,
        force_full_scan,
        #[cfg(test)]
        &mut cache.probe,
    );
    let sessions_budget =
        LOCAL_IMPORT_RETAINED_PATH_CAP.saturating_sub(cache.transfers.retained_paths.len());
    let sessions_fingerprint = fingerprint_import_directory(
        &sessions,
        ImportArtifactDirectory::Sessions,
        &mut cache.sessions,
        sessions_budget,
        force_full_scan,
        #[cfg(test)]
        &mut cache.probe,
    );
    let combined_count = cache
        .transfers
        .recognized_count
        .saturating_add(cache.sessions.recognized_count);
    let global_overflow = combined_count > LOCAL_IMPORT_RETAINED_PATH_CAP;
    cache.force_full_scan = global_overflow || cache.transfers.overflow || cache.sessions.overflow;
    let mut hasher = DefaultHasher::new();
    0_u8.hash(&mut hasher);
    transfers_fingerprint.digest.hash(&mut hasher);
    1_u8.hash(&mut hasher);
    sessions_fingerprint.digest.hash(&mut hasher);
    LocalImportFilesFingerprint {
        digest: hasher.finish(),
        reliable: transfers_fingerprint.reliable
            && sessions_fingerprint.reliable
            && !global_overflow,
    }
}

#[cfg(test)]
pub(in crate::app) fn local_import_files_fingerprint_for_test(
    cache: &mut LocalImportFilesFingerprintCache,
) -> (u64, bool) {
    let fingerprint = local_import_files_fingerprint(cache);
    (fingerprint.digest, fingerprint.reliable)
}

#[cfg(test)]
pub(in crate::app) fn stable_import_cache_key_for_test(
    before: Option<(u64, bool)>,
    after: Option<(u64, bool)>,
) -> Option<Option<u64>> {
    let fingerprint = |(digest, reliable)| LocalImportFilesFingerprint { digest, reliable };
    stable_import_cache_key(before.map(fingerprint), after.map(fingerprint))
}

fn fingerprint_import_directory(
    path: &Path,
    kind: ImportArtifactDirectory,
    cache: &mut CachedImportDirectory,
    retained_path_budget: usize,
    force_scan: bool,
    #[cfg(test)] probe: &mut LocalImportFingerprintProbe,
) -> LocalImportFilesFingerprint {
    let metadata = directory_metadata_state(
        path,
        #[cfg(test)]
        probe,
    );
    let path_changed = cache.path.as_deref() != Some(path);
    let metadata_changed = cache.metadata != Some(metadata);
    if path_changed {
        *cache = CachedImportDirectory::default();
        cache.path = Some(path.to_path_buf());
    }
    cache.metadata = Some(metadata);

    let ImportMetadataState::Present(signature) = metadata else {
        cache.retained_paths = Vec::new();
        cache.recognized_count = 0;
        cache.membership_valid = matches!(
            metadata,
            ImportMetadataState::Error(std::io::ErrorKind::NotFound)
        );
        cache.overflow = false;
        #[cfg(windows)]
        {
            cache.directory_change_time = None;
        }
        return hash_import_directory_state(
            path,
            &ImportEntryAccumulator::default(),
            cache.membership_valid,
        );
    };
    if !signature.is_dir {
        cache.retained_paths = Vec::new();
        cache.recognized_count = 0;
        cache.membership_valid = true;
        cache.overflow = false;
        #[cfg(windows)]
        {
            cache.directory_change_time = None;
        }
        return hash_import_directory_state(path, &ImportEntryAccumulator::default(), true);
    }

    #[cfg(windows)]
    let (membership_changed, generation_reliable) = {
        if metadata_changed {
            cache.directory_change_time = None;
        }
        match windows_directory_change_time(
            path,
            #[cfg(test)]
            probe,
        ) {
            Ok(change_time) => {
                let previous = cache.directory_change_time.replace(change_time);
                (previous != Some(change_time), true)
            }
            Err(_) => {
                cache.directory_change_time = None;
                (true, false)
            }
        }
    };
    #[cfg(not(windows))]
    let (membership_changed, generation_reliable) = (metadata_changed, true);

    let mut fingerprint = if force_scan
        || path_changed
        || membership_changed
        || !cache.membership_valid
        || cache.overflow
    {
        scan_import_directory(
            path,
            kind,
            cache,
            retained_path_budget,
            #[cfg(test)]
            probe,
        )
    } else {
        fingerprint_retained_import_paths(
            path,
            cache,
            #[cfg(test)]
            probe,
        )
    };
    fingerprint.reliable &= generation_reliable;
    fingerprint
}

fn scan_import_directory(
    path: &Path,
    kind: ImportArtifactDirectory,
    cache: &mut CachedImportDirectory,
    retained_path_budget: usize,
    #[cfg(test)] probe: &mut LocalImportFingerprintProbe,
) -> LocalImportFilesFingerprint {
    #[cfg(test)]
    {
        probe.read_dir_calls = probe.read_dir_calls.wrapping_add(1);
    }
    #[cfg(test)]
    let read_dir = match cache.next_read_dir_error.take() {
        Some(kind) => Err(std::io::Error::from(kind)),
        None => std::fs::read_dir(path),
    };
    #[cfg(not(test))]
    let read_dir = std::fs::read_dir(path);
    let entries = match read_dir {
        Ok(entries) => entries,
        Err(error) => {
            #[cfg(test)]
            {
                probe.read_dir_errors = probe.read_dir_errors.wrapping_add(1);
            }
            cache.retained_paths = Vec::new();
            cache.recognized_count = 0;
            cache.membership_valid = false;
            cache.overflow = false;
            let mut accumulator = ImportEntryAccumulator::default();
            accumulator.push(hash_value(&(0_u8, error.kind())));
            return hash_import_directory_state(path, &accumulator, false);
        }
    };

    let mut accumulator = ImportEntryAccumulator::default();
    let mut retained = Vec::new();
    let mut recognized_count = 0_usize;
    let mut overflow = false;
    let mut reliable = true;
    for entry in entries {
        #[cfg(test)]
        {
            probe.directory_entries_visited = probe.directory_entries_visited.wrapping_add(1);
        }
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                reliable = false;
                accumulator.push(hash_value(&(1_u8, error.kind())));
                continue;
            }
        };
        let file_name = entry.file_name();
        if !is_recognized_import_artifact(&file_name, kind) {
            continue;
        }
        recognized_count = recognized_count.saturating_add(1);
        let entry_path = entry.path();
        let artifact_metadata = artifact_metadata_state(
            &entry_path,
            #[cfg(test)]
            probe,
        );
        if matches!(artifact_metadata, ImportMetadataState::Error(_)) {
            reliable = false;
        }
        accumulator.push(hash_import_entry(&file_name, artifact_metadata));
        if !overflow && retained.len() < retained_path_budget {
            retained.push(entry_path);
        } else {
            overflow = true;
            retained = Vec::new();
        }
    }
    retained.sort_unstable();
    cache.retained_paths = retained;
    cache.recognized_count = recognized_count;
    cache.membership_valid = reliable;
    cache.overflow = overflow;
    hash_import_directory_state(path, &accumulator, reliable && !overflow)
}

fn fingerprint_retained_import_paths(
    path: &Path,
    cache: &mut CachedImportDirectory,
    #[cfg(test)] probe: &mut LocalImportFingerprintProbe,
) -> LocalImportFilesFingerprint {
    let mut accumulator = ImportEntryAccumulator::default();
    let mut reliable = true;
    for entry_path in &cache.retained_paths {
        let metadata = artifact_metadata_state(
            entry_path,
            #[cfg(test)]
            probe,
        );
        if matches!(metadata, ImportMetadataState::Error(_)) {
            reliable = false;
        }
        let Some(file_name) = entry_path.file_name() else {
            reliable = false;
            continue;
        };
        accumulator.push(hash_import_entry(file_name, metadata));
    }
    if !reliable {
        cache.membership_valid = false;
    } else {
        cache.recognized_count = cache.retained_paths.len();
    }
    hash_import_directory_state(path, &accumulator, reliable)
}

fn hash_import_directory_state(
    path: &Path,
    entries: &ImportEntryAccumulator,
    reliable: bool,
) -> LocalImportFilesFingerprint {
    let mut hasher = DefaultHasher::new();
    path.hash(&mut hasher);
    entries.hash_into(&mut hasher);
    LocalImportFilesFingerprint {
        digest: hasher.finish(),
        reliable,
    }
}

fn directory_metadata_state(
    path: &Path,
    #[cfg(test)] probe: &mut LocalImportFingerprintProbe,
) -> ImportMetadataState {
    #[cfg(test)]
    {
        probe.directory_metadata_stats = probe.directory_metadata_stats.wrapping_add(1);
    }
    metadata_state(std::fs::metadata(path))
}

fn artifact_metadata_state(
    path: &Path,
    #[cfg(test)] probe: &mut LocalImportFingerprintProbe,
) -> ImportMetadataState {
    #[cfg(test)]
    {
        probe.artifact_metadata_stats = probe.artifact_metadata_stats.wrapping_add(1);
    }
    let metadata = std::fs::symlink_metadata(path);
    #[cfg(windows)]
    {
        match metadata {
            Ok(metadata) => match crate::util::safe_fs::windows_file_change_time(path) {
                Ok(change_time) => ImportMetadataState::Present(import_metadata_signature(
                    &metadata,
                    Some(change_time),
                )),
                Err(error) => ImportMetadataState::Error(error.kind()),
            },
            Err(error) => ImportMetadataState::Error(error.kind()),
        }
    }
    #[cfg(not(windows))]
    {
        metadata_state(metadata)
    }
}

fn metadata_state(result: std::io::Result<std::fs::Metadata>) -> ImportMetadataState {
    match result {
        Ok(metadata) => ImportMetadataState::Present(import_metadata_signature(&metadata, None)),
        Err(error) => ImportMetadataState::Error(error.kind()),
    }
}

fn import_metadata_signature(
    metadata: &std::fs::Metadata,
    artifact_change_time: Option<i64>,
) -> ImportMetadataSignature {
    let modified = match metadata.modified() {
        Ok(modified) => match modified.duration_since(UNIX_EPOCH) {
            Ok(duration) => ImportModifiedTimeSignature::SinceEpoch {
                secs: duration.as_secs(),
                nanos: duration.subsec_nanos(),
            },
            Err(error) => ImportModifiedTimeSignature::BeforeEpoch {
                secs: error.duration().as_secs(),
                nanos: error.duration().subsec_nanos(),
            },
        },
        Err(error) => ImportModifiedTimeSignature::Unavailable(error.kind()),
    };
    ImportMetadataSignature {
        len: metadata.len(),
        is_file: metadata.is_file(),
        is_dir: metadata.is_dir(),
        is_symlink: metadata.file_type().is_symlink(),
        readonly: metadata.permissions().readonly(),
        modified,
        platform: import_platform_metadata_signature(metadata, artifact_change_time),
    }
}

#[cfg(unix)]
fn import_platform_metadata_signature(
    metadata: &std::fs::Metadata,
    _artifact_change_time: Option<i64>,
) -> ImportPlatformMetadataSignature {
    use std::os::unix::fs::MetadataExt;

    ImportPlatformMetadataSignature {
        device: metadata.dev(),
        inode: metadata.ino(),
        mode: metadata.mode(),
        links: metadata.nlink(),
        changed_secs: metadata.ctime(),
        changed_nanos: metadata.ctime_nsec(),
    }
}

#[cfg(windows)]
fn import_platform_metadata_signature(
    metadata: &std::fs::Metadata,
    artifact_change_time: Option<i64>,
) -> ImportPlatformMetadataSignature {
    use std::os::windows::fs::MetadataExt;

    ImportPlatformMetadataSignature {
        attributes: metadata.file_attributes(),
        creation_time: metadata.creation_time(),
        last_write_time: metadata.last_write_time(),
        artifact_change_time,
    }
}

#[cfg(not(any(unix, windows)))]
fn import_platform_metadata_signature(
    _metadata: &std::fs::Metadata,
    _artifact_change_time: Option<i64>,
) -> ImportPlatformMetadataSignature {
    ImportPlatformMetadataSignature
}

fn is_recognized_import_artifact(name: &OsStr, kind: ImportArtifactDirectory) -> bool {
    let Some(name) = name.to_str() else {
        return false;
    };
    name.ends_with(".json")
        || matches!(kind, ImportArtifactDirectory::Transfers) && name.ends_with(".journal.jsonl")
}

fn hash_import_entry(name: &OsStr, metadata: ImportMetadataState) -> u64 {
    hash_value(&(name, metadata))
}

#[cfg(windows)]
fn windows_directory_change_time(
    path: &Path,
    #[cfg(test)] probe: &mut LocalImportFingerprintProbe,
) -> std::io::Result<i64> {
    #[cfg(test)]
    {
        probe.directory_generation_queries = probe.directory_generation_queries.wrapping_add(1);
    }
    crate::util::safe_fs::windows_directory_change_time(path)
}

fn hash_value(value: &impl Hash) -> u64 {
    let mut hasher = DefaultHasher::new();
    value.hash(&mut hasher);
    hasher.finish()
}
