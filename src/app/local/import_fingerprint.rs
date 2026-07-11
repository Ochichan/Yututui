//! Bounded filesystem fingerprints for Local Deck import artifacts.

use std::collections::{HashMap, hash_map::DefaultHasher};
use std::hash::{Hash, Hasher};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

/// Enough for hundreds of import sessions while keeping retained path/signature state below a
/// small, fixed ceiling even if the transfers directory is unexpectedly large.
const LOCAL_IMPORT_CONTENT_CACHE_CAP: usize = 4096;

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
}

#[cfg(not(any(unix, windows)))]
#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
struct ImportPlatformMetadataSignature;

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
struct ImportFileMetadataSignature {
    len: u64,
    is_file: bool,
    is_dir: bool,
    readonly: bool,
    modified: ImportModifiedTimeSignature,
    platform: ImportPlatformMetadataSignature,
}

struct CachedImportFileDigest {
    metadata: ImportFileMetadataSignature,
    content_digest: u64,
    seen_generation: u64,
}

/// Persistent metadata-to-content-digest memoization owned by one `LocalMode`. Metadata is still
/// sampled on every import render; only files whose signatures changed are opened and re-hashed.
pub(in crate::app) struct LocalImportFilesFingerprintCache {
    files: HashMap<PathBuf, CachedImportFileDigest>,
    generation: u64,
    capacity: usize,
    #[cfg(test)]
    content_open_attempts: u64,
    #[cfg(test)]
    content_bytes_read: u64,
}

impl Default for LocalImportFilesFingerprintCache {
    fn default() -> Self {
        Self {
            files: HashMap::new(),
            generation: 0,
            capacity: LOCAL_IMPORT_CONTENT_CACHE_CAP,
            #[cfg(test)]
            content_open_attempts: 0,
            #[cfg(test)]
            content_bytes_read: 0,
        }
    }
}

impl LocalImportFilesFingerprintCache {
    fn begin_scan(&mut self) {
        if self.generation == u64::MAX {
            for cached in self.files.values_mut() {
                cached.seen_generation = 0;
            }
            self.generation = 1;
        } else {
            self.generation += 1;
        }
    }

    fn finish_scan(&mut self) {
        let generation = self.generation;
        self.files
            .retain(|_, cached| cached.seen_generation == generation);
    }

    fn reusable_digest(
        &mut self,
        path: &Path,
        metadata: ImportFileMetadataSignature,
    ) -> Option<u64> {
        let cached = self.files.get_mut(path)?;
        if cached.metadata != metadata {
            return None;
        }
        cached.seen_generation = self.generation;
        Some(cached.content_digest)
    }

    fn remove(&mut self, path: &Path) {
        self.files.remove(path);
    }

    fn store(&mut self, path: PathBuf, metadata: ImportFileMetadataSignature, content_digest: u64) {
        if self.capacity == 0 {
            return;
        }
        if self.files.len() >= self.capacity
            && let Some(stale) = self.files.iter().find_map(|(path, cached)| {
                (cached.seen_generation != self.generation).then(|| path.clone())
            })
        {
            self.files.remove(&stale);
        }
        if self.files.len() < self.capacity {
            self.files.insert(
                path,
                CachedImportFileDigest {
                    metadata,
                    content_digest,
                    seen_generation: self.generation,
                },
            );
        }
    }

    fn record_content_open(&mut self) {
        #[cfg(test)]
        {
            self.content_open_attempts = self.content_open_attempts.wrapping_add(1);
        }
    }

    fn record_content_bytes(&mut self, bytes: usize) {
        #[cfg(not(test))]
        let _ = bytes;
        #[cfg(test)]
        {
            self.content_bytes_read = self.content_bytes_read.wrapping_add(bytes as u64);
        }
    }

    #[cfg(test)]
    fn set_capacity(&mut self, capacity: usize) {
        self.capacity = capacity;
        while self.files.len() > capacity {
            let Some(path) = self.files.keys().next().cloned() else {
                break;
            };
            self.files.remove(&path);
        }
    }
}

pub(super) fn local_import_files_fingerprint(cache: &mut LocalImportFilesFingerprintCache) -> u64 {
    let Some(data_dir) = crate::paths::data_dir() else {
        return fingerprint_import_directories(std::iter::empty::<&Path>(), cache);
    };
    let transfers = data_dir.join("transfers");
    let sessions = transfers.join("sessions");
    fingerprint_import_directories([transfers.as_path(), sessions.as_path()], cache)
}

fn fingerprint_import_directories<'a>(
    paths: impl IntoIterator<Item = &'a Path>,
    cache: &mut LocalImportFilesFingerprintCache,
) -> u64 {
    cache.begin_scan();
    let mut hasher = DefaultHasher::new();
    for path in paths {
        hash_import_directory(path, &mut hasher, cache);
    }
    cache.finish_scan();
    hasher.finish()
}

fn hash_import_directory(
    path: &Path,
    hasher: &mut impl Hasher,
    cache: &mut LocalImportFilesFingerprintCache,
) {
    path.hash(hasher);
    let Ok(entries) = std::fs::read_dir(path) else {
        0_u8.hash(hasher);
        return;
    };
    let mut entry_count = 0_u64;
    let mut entry_sum = 0_u64;
    let mut entry_xor = 0_u64;
    for entry in entries.flatten() {
        let mut entry_hasher = DefaultHasher::new();
        entry.file_name().hash(&mut entry_hasher);
        match entry.metadata() {
            Ok(metadata) => {
                let metadata_signature = import_file_metadata_signature(&metadata);
                metadata_signature.hash(&mut entry_hasher);
                if metadata.is_file() {
                    let entry_path = entry.path();
                    if let Some(content_digest) =
                        cache.reusable_digest(&entry_path, metadata_signature)
                    {
                        2_u8.hash(&mut entry_hasher);
                        content_digest.hash(&mut entry_hasher);
                    } else {
                        // Failed opens/reads are deliberately not memoized: an unchanged metadata
                        // signature must not hide recovery from a transient permission or I/O error.
                        cache.remove(&entry_path);
                        match read_import_file_digest(&entry_path, cache) {
                            ImportFileDigestRead::Complete(content_digest) => {
                                2_u8.hash(&mut entry_hasher);
                                content_digest.hash(&mut entry_hasher);
                                cache.store(entry_path, metadata_signature, content_digest);
                            }
                            ImportFileDigestRead::ReadError {
                                partial_digest,
                                kind,
                            } => {
                                2_u8.hash(&mut entry_hasher);
                                partial_digest.hash(&mut entry_hasher);
                                3_u8.hash(&mut entry_hasher);
                                kind.hash(&mut entry_hasher);
                            }
                            ImportFileDigestRead::OpenError(kind) => {
                                4_u8.hash(&mut entry_hasher);
                                kind.hash(&mut entry_hasher);
                            }
                        }
                    }
                }
            }
            Err(error) => {
                error.kind().hash(&mut entry_hasher);
            }
        }
        let fingerprint = entry_hasher.finish();
        entry_count += 1;
        entry_sum = entry_sum.wrapping_add(fingerprint);
        entry_xor ^= fingerprint.rotate_left((fingerprint & 63) as u32);
    }
    entry_count.hash(hasher);
    entry_sum.hash(hasher);
    entry_xor.hash(hasher);
}

fn import_file_metadata_signature(metadata: &std::fs::Metadata) -> ImportFileMetadataSignature {
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
    ImportFileMetadataSignature {
        len: metadata.len(),
        is_file: metadata.is_file(),
        is_dir: metadata.is_dir(),
        readonly: metadata.permissions().readonly(),
        modified,
        platform: import_platform_metadata_signature(metadata),
    }
}

#[cfg(unix)]
fn import_platform_metadata_signature(
    metadata: &std::fs::Metadata,
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
) -> ImportPlatformMetadataSignature {
    use std::os::windows::fs::MetadataExt;

    ImportPlatformMetadataSignature {
        attributes: metadata.file_attributes(),
        creation_time: metadata.creation_time(),
        last_write_time: metadata.last_write_time(),
    }
}

#[cfg(not(any(unix, windows)))]
fn import_platform_metadata_signature(
    _metadata: &std::fs::Metadata,
) -> ImportPlatformMetadataSignature {
    ImportPlatformMetadataSignature
}

enum ImportFileDigestRead {
    Complete(u64),
    ReadError {
        partial_digest: u64,
        kind: std::io::ErrorKind,
    },
    OpenError(std::io::ErrorKind),
}

fn read_import_file_digest(
    path: &Path,
    cache: &mut LocalImportFilesFingerprintCache,
) -> ImportFileDigestRead {
    cache.record_content_open();
    let mut file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) => return ImportFileDigestRead::OpenError(error.kind()),
    };
    let mut content_hasher = DefaultHasher::new();
    let mut buffer = [0_u8; 8192];
    loop {
        match file.read(&mut buffer) {
            Ok(0) => return ImportFileDigestRead::Complete(content_hasher.finish()),
            Ok(read) => {
                cache.record_content_bytes(read);
                content_hasher.write(&buffer[..read]);
            }
            Err(error) => {
                return ImportFileDigestRead::ReadError {
                    partial_digest: content_hasher.finish(),
                    kind: error.kind(),
                };
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_TEST_DIRECTORY: AtomicU64 = AtomicU64::new(0);

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            let sequence = NEXT_TEST_DIRECTORY.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "yututui-import-fingerprint-{}-{sequence}",
                std::process::id()
            ));
            std::fs::create_dir_all(&path).expect("create import fingerprint test directory");
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn fingerprint(path: &Path, cache: &mut LocalImportFilesFingerprintCache) -> u64 {
        fingerprint_import_directories([path], cache)
    }

    #[test]
    fn unchanged_import_artifact_reuses_digest_with_zero_content_io() {
        let directory = TestDirectory::new();
        let artifact = directory.path().join("session.report.json");
        let initial = vec![b'a'; 16_385];
        std::fs::write(&artifact, &initial).expect("write initial artifact");
        let mut cache = LocalImportFilesFingerprintCache::default();

        let first = fingerprint(directory.path(), &mut cache);
        assert_eq!(cache.content_open_attempts, 1);
        assert_eq!(cache.content_bytes_read, initial.len() as u64);
        assert_eq!(cache.files.len(), 1);
        let io_after_first_scan = (cache.content_open_attempts, cache.content_bytes_read);

        let unchanged = fingerprint(directory.path(), &mut cache);
        assert_eq!(unchanged, first);
        assert_eq!(
            (cache.content_open_attempts, cache.content_bytes_read),
            io_after_first_scan,
            "an unchanged metadata signature must cause zero opens and zero content bytes read"
        );

        let modified = vec![b'b'; initial.len() + 1];
        std::fs::write(&artifact, &modified).expect("modify artifact");
        let after_modify = fingerprint(directory.path(), &mut cache);
        assert_ne!(after_modify, unchanged);
        assert_eq!(cache.content_open_attempts, io_after_first_scan.0 + 1);
        assert_eq!(
            cache.content_bytes_read,
            io_after_first_scan.1 + modified.len() as u64
        );

        let io_after_modify = (cache.content_open_attempts, cache.content_bytes_read);
        assert_eq!(fingerprint(directory.path(), &mut cache), after_modify);
        assert_eq!(
            (cache.content_open_attempts, cache.content_bytes_read),
            io_after_modify,
            "the digest recalculated after a modification must also be reusable"
        );

        std::fs::remove_file(&artifact).expect("delete artifact");
        assert_ne!(fingerprint(directory.path(), &mut cache), after_modify);
        assert!(cache.files.is_empty(), "deleted artifacts must be pruned");
        assert_eq!(
            (cache.content_open_attempts, cache.content_bytes_read),
            io_after_modify,
            "deletion invalidation only needs directory metadata, not a content read"
        );
    }

    #[cfg(unix)]
    #[test]
    fn unix_change_identity_catches_same_size_rewrite_with_restored_mtime() {
        let directory = TestDirectory::new();
        let artifact = directory.path().join("session.report.json");
        std::fs::write(&artifact, b"{}").expect("write initial artifact");
        let mut cache = LocalImportFilesFingerprintCache::default();
        let before = fingerprint(directory.path(), &mut cache);
        let original_modified = std::fs::metadata(&artifact)
            .and_then(|metadata| metadata.modified())
            .expect("read original timestamp");

        std::fs::write(&artifact, b"[]").expect("rewrite artifact at the same size");
        std::fs::File::options()
            .write(true)
            .open(&artifact)
            .and_then(|file| {
                file.set_times(std::fs::FileTimes::new().set_modified(original_modified))
            })
            .expect("restore original mtime");

        let after = fingerprint(directory.path(), &mut cache);
        assert_ne!(after, before);
        assert_eq!(cache.content_open_attempts, 2);
        assert_eq!(cache.content_bytes_read, 4);
    }

    #[test]
    fn import_digest_cache_is_bounded_and_prunes_unseen_paths() {
        let directory = TestDirectory::new();
        for index in 0..3 {
            std::fs::write(
                directory.path().join(format!("{index}.json")),
                [index as u8],
            )
            .expect("write bounded-cache artifact");
        }
        let mut cache = LocalImportFilesFingerprintCache::default();
        cache.set_capacity(2);

        fingerprint(directory.path(), &mut cache);
        assert_eq!(cache.files.len(), 2);

        for index in 0..3 {
            std::fs::remove_file(directory.path().join(format!("{index}.json")))
                .expect("remove bounded-cache artifact");
        }
        fingerprint(directory.path(), &mut cache);
        assert!(cache.files.is_empty());
    }
}
