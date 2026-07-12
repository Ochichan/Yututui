use std::collections::VecDeque;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context as _, Result, bail};

const RETAINED_FILE_CAP: usize = 512;
const RETAINED_SOURCE_BYTES_CAP: u64 = 8 * 1024 * 1024 * 1024;
const RETAINED_SCAN_ENTRY_CAP: usize = 2_048;
const RETAINED_SCAN_DEPTH_CAP: usize = 3;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct RetainedImportInventory {
    pub(super) files: usize,
    pub(super) bytes: u64,
    pub(super) over_limit: bool,
}

pub(super) fn ensure_retained_import_capacity(root: &Path) -> Result<()> {
    let inventory = retained_import_inventory(root)?;
    if inventory.over_limit
        || inventory.files.saturating_add(1) > RETAINED_FILE_CAP
        || inventory.bytes >= RETAINED_SOURCE_BYTES_CAP
    {
        bail!(
            "import admission rejected: retained artifact storage is full or busy ({} files/{RETAINED_FILE_CAP}, {} bytes/{RETAINED_SOURCE_BYTES_CAP}); review retained import work before retrying",
            inventory.files,
            inventory.bytes
        );
    }
    Ok(())
}

pub(super) fn retained_import_inventory(root: &Path) -> Result<RetainedImportInventory> {
    let base = root.join(".yututui-inbox");
    let metadata = match std::fs::symlink_metadata(&base) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(RetainedImportInventory::default());
        }
        Err(error) => return Err(error.into()),
    };
    reject_directory_component(&base, &metadata)?;

    let mut inventory = RetainedImportInventory::default();
    let mut scanned = 0_usize;
    let mut pending = VecDeque::from([(base, 0_usize)]);
    while let Some((directory, depth)) = pending.pop_front() {
        for entry in std::fs::read_dir(&directory)? {
            scanned = scanned.saturating_add(1);
            if scanned > RETAINED_SCAN_ENTRY_CAP {
                bail!(
                    "retained import inventory exceeds its {RETAINED_SCAN_ENTRY_CAP} entry scan cap; admission is busy"
                );
            }
            let entry = entry?;
            let path = entry.path();
            let metadata = std::fs::symlink_metadata(&path)?;
            if metadata.is_dir() && !is_link_or_reparse(&metadata) {
                if depth >= RETAINED_SCAN_DEPTH_CAP {
                    bail!(
                        "retained import inventory exceeds its directory depth cap at {}",
                        path.display()
                    );
                }
                pending.push_back((path, depth + 1));
                continue;
            }
            if is_link_or_reparse(&metadata) || !metadata.is_file() {
                bail!(
                    "retained import inventory contains an unsafe entry: {}",
                    path.display()
                );
            }
            inventory.files = inventory.files.saturating_add(1);
            inventory.bytes = inventory
                .bytes
                .checked_add(metadata.len())
                .ok_or_else(|| anyhow::anyhow!("retained import inventory size overflow"))?;
            if inventory.files > RETAINED_FILE_CAP || inventory.bytes > RETAINED_SOURCE_BYTES_CAP {
                inventory.over_limit = true;
                return Ok(inventory);
            }
        }
    }
    Ok(inventory)
}

pub(super) fn reclaim_import_file(
    root: &Path,
    path: &Path,
    expected: crate::util::safe_fs::FileObjectId,
) -> Result<bool> {
    let base = std::fs::canonicalize(root.join(".yututui-inbox"))?;
    let parent_path = path
        .parent()
        .context("retained import file has no parent")?;
    let parent = std::fs::canonicalize(parent_path)?;
    let relative = parent.strip_prefix(&base).with_context(|| {
        format!(
            "retained import file escaped private inbox: {}",
            path.display()
        )
    })?;
    let pinned = crate::util::safe_fs::PinnedDir::open_private_existing(&base, relative)?;
    let name = path
        .file_name()
        .context("retained import file has no basename")?;
    let generation = match pinned.open_existing_child_readonly(name, expected) {
        Ok(generation) => generation,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    generation.remove_from_private_parent()?;
    Ok(true)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ImportInboxDirs {
    pub(super) incoming: PathBuf,
    pub(super) complete: PathBuf,
    pub(super) failed: PathBuf,
}

pub(super) fn import_inbox_dirs(root: &Path, session_id: &str) -> Result<ImportInboxDirs> {
    let session = safe_import_session_component(session_id)?;
    let base = root.join(".yututui-inbox").join(session);
    Ok(ImportInboxDirs {
        incoming: base.join("incoming"),
        complete: base.join("complete"),
        failed: base.join("failed"),
    })
}

pub(super) fn prepare_import_inbox_dirs(root: &Path, session_id: &str) -> Result<ImportInboxDirs> {
    let dirs = import_inbox_dirs(root, session_id)?;
    crate::util::safe_fs::ensure_dir_durable(root)
        .with_context(|| format!("create download root {}", root.display()))?;
    let private_base = root.join(".yututui-inbox");
    crate::util::safe_fs::ensure_private_dir_durable(&private_base)
        .with_context(|| format!("create private import root {}", private_base.display()))?;
    let session_dir = dirs
        .incoming
        .parent()
        .context("import incoming directory has no session parent")?;
    crate::util::safe_fs::ensure_private_dir_durable(session_dir)
        .with_context(|| format!("create private import session {}", session_dir.display()))?;
    for directory in [&dirs.incoming, &dirs.complete, &dirs.failed] {
        crate::util::safe_fs::ensure_private_dir_durable(directory)
            .with_context(|| format!("create private import directory {}", directory.display()))?;
    }
    validate_import_inbox_dirs(root, &dirs)?;
    Ok(dirs)
}

pub(super) fn validate_real_directory_chain(root: &Path, target: &Path) -> Result<()> {
    walk_real_directory_chain(root, target)
}

pub(super) fn validate_downloaded_path(
    path: &Path,
    directory: &Path,
    import_root: Option<&Path>,
) -> Result<std::fs::Metadata> {
    if let Some(root) = import_root {
        validate_real_directory_chain(root, directory)?;
    }
    let file = crate::util::safe_fs::open_regular_no_symlink(path)
        .with_context(|| format!("downloaded file not found: {}", path.display()))?;
    let metadata = file.metadata()?;
    let canonical_directory = std::fs::canonicalize(directory).with_context(|| {
        format!(
            "download directory could not be resolved: {}",
            directory.display()
        )
    })?;
    let resolved = std::fs::canonicalize(path)
        .with_context(|| format!("downloaded path could not be resolved: {}", path.display()))?;
    if !resolved.starts_with(&canonical_directory) {
        bail!(
            "downloaded path escaped the download directory: {}",
            path.display()
        );
    }
    if let Some(root) = import_root {
        let canonical_root = std::fs::canonicalize(root)?;
        if !resolved.starts_with(canonical_root) {
            bail!(
                "downloaded path escaped the import root: {}",
                path.display()
            );
        }
    }
    Ok(metadata)
}

pub(super) fn find_retryable_audio(directory: &Path, video_id: &str) -> Result<Option<PathBuf>> {
    const ENTRY_CAP: usize = 256;
    let mut found = None;
    for (index, entry) in std::fs::read_dir(directory)?.enumerate() {
        if index >= ENTRY_CAP {
            bail!("import inbox exceeds its {ENTRY_CAP} entry scan cap");
        }
        let entry = entry?;
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if name.starts_with('.') || name.ends_with(".ytt.json") {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
            continue;
        };
        let Some((_, embedded_id)) = crate::api::Song::parse_embedded_id(stem) else {
            continue;
        };
        if embedded_id != video_id || crate::util::safe_fs::open_regular_no_symlink(&path).is_err()
        {
            continue;
        }
        if found.replace(path).is_some() {
            bail!("multiple retryable audio files match import video id {video_id}");
        }
    }
    Ok(found)
}

fn validate_import_inbox_dirs(root: &Path, dirs: &ImportInboxDirs) -> Result<()> {
    let base = root.join(".yututui-inbox");
    let canonical_base = std::fs::canonicalize(&base)
        .with_context(|| format!("canonicalize private import root {}", base.display()))?;
    for directory in [&dirs.incoming, &dirs.complete, &dirs.failed] {
        validate_real_directory_chain(root, directory)?;
        let relative = directory.strip_prefix(&base).with_context(|| {
            format!(
                "private import directory escaped root: {}",
                directory.display()
            )
        })?;
        crate::util::safe_fs::PinnedDir::open_private_existing(&canonical_base, relative)
            .with_context(|| {
                format!("validate private import directory {}", directory.display())
            })?;
    }
    Ok(())
}

fn walk_real_directory_chain(root: &Path, target: &Path) -> Result<()> {
    let relative = target.strip_prefix(root).with_context(|| {
        format!(
            "import directory escaped download root: {}",
            target.display()
        )
    })?;
    let canonical_root = std::fs::canonicalize(root)
        .with_context(|| format!("canonicalize download root {}", root.display()))?;
    let mut current = root.to_path_buf();
    for component in relative.components() {
        let Component::Normal(component) = component else {
            if matches!(component, Component::CurDir) {
                continue;
            }
            bail!("invalid import directory path: {}", target.display());
        };
        current.push(component);
        let metadata = match std::fs::symlink_metadata(&current) {
            Ok(metadata) => metadata,
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("inspect import directory {}", current.display()));
            }
        };
        reject_directory_component(&current, &metadata)?;
        let resolved = std::fs::canonicalize(&current)?;
        if !resolved.starts_with(&canonical_root) {
            bail!(
                "import directory escaped download root: {}",
                current.display()
            );
        }
    }
    Ok(())
}

fn reject_directory_component(path: &Path, metadata: &std::fs::Metadata) -> Result<()> {
    if is_link_or_reparse(metadata) || !metadata.is_dir() {
        bail!(
            "refusing symlink or non-directory import inbox component: {}",
            path.display()
        );
    }
    Ok(())
}

fn is_link_or_reparse(metadata: &std::fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt as _;
        metadata.file_type().is_symlink() || metadata.file_attributes() & 0x0000_0400 != 0
    }
    #[cfg(not(windows))]
    {
        metadata.file_type().is_symlink()
    }
}

fn safe_import_session_component(session_id: &str) -> Result<&str> {
    let trimmed = session_id.trim();
    if trimmed.is_empty()
        || trimmed == "."
        || trimmed == ".."
        || !trimmed
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    {
        bail!("invalid import session id for inbox path: {session_id:?}");
    }
    Ok(trimmed)
}

#[cfg(test)]
mod retained_tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn preparation_preserves_public_root_mode_and_privatises_every_inbox_level() {
        use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};

        let root = temp_root("private-modes");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755)).unwrap();

        let dirs = prepare_import_inbox_dirs(&root, "session-a").unwrap();

        assert_eq!(std::fs::metadata(&root).unwrap().mode() & 0o777, 0o755);
        for directory in [
            root.join(".yututui-inbox"),
            root.join(".yututui-inbox/session-a"),
            dirs.incoming,
            dirs.complete,
            dirs.failed,
        ] {
            assert_eq!(
                std::fs::metadata(&directory).unwrap().mode() & 0o777,
                0o700,
                "{} was not private",
                directory.display()
            );
        }
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn exact_reclamation_repeatedly_restores_admission_capacity() {
        let root = temp_root("reclaim-capacity");
        let dirs = prepare_import_inbox_dirs(&root, "session-a").unwrap();

        for index in 0..16 {
            ensure_retained_import_capacity(&root).unwrap();
            let path = dirs.incoming.join(format!("raw-{index}.part"));
            std::fs::write(&path, b"retained raw audio").unwrap();
            let file = crate::util::safe_fs::open_regular_no_symlink(&path).unwrap();
            let expected = crate::util::safe_fs::file_object_id(&file).unwrap();
            assert_eq!(retained_import_inventory(&root).unwrap().files, 1);

            assert!(reclaim_import_file(&root, &path, expected).unwrap());
            assert_eq!(retained_import_inventory(&root).unwrap().files, 0);
        }
        ensure_retained_import_capacity(&root).unwrap();
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn reclamation_preserves_a_foreign_replacement() {
        let root = temp_root("reclaim-replacement");
        let dirs = prepare_import_inbox_dirs(&root, "session-a").unwrap();
        let path = dirs.incoming.join("raw.part");
        std::fs::write(&path, b"owned generation").unwrap();
        let file = crate::util::safe_fs::open_regular_no_symlink(&path).unwrap();
        let expected = crate::util::safe_fs::file_object_id(&file).unwrap();
        drop(file);
        std::fs::rename(&path, root.join("displaced-owned-generation")).unwrap();
        std::fs::write(&path, b"foreign replacement").unwrap();

        let error = reclaim_import_file(&root, &path, expected).unwrap_err();
        assert!(format!("{error:#}").contains("does not match its persisted object identity"));
        assert_eq!(std::fs::read(&path).unwrap(), b"foreign replacement");

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn global_count_cap_spans_sessions_and_rejects_explicitly() {
        let root = temp_root("global-count");
        for session in ["session-a", "session-b"] {
            let incoming = root.join(".yututui-inbox").join(session).join("incoming");
            std::fs::create_dir_all(&incoming).unwrap();
            for index in 0..(RETAINED_FILE_CAP / 2) {
                std::fs::write(incoming.join(format!("retained-{index}.part")), []).unwrap();
            }
        }
        let inventory = retained_import_inventory(&root).unwrap();
        assert_eq!(inventory.files, RETAINED_FILE_CAP);
        let error = ensure_retained_import_capacity(&root).unwrap_err();
        assert!(format!("{error:#}").contains("retained artifact storage is full or busy"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn global_source_byte_cap_rejects_sparse_retained_file() {
        let root = temp_root("global-bytes");
        let incoming = root
            .join(".yututui-inbox")
            .join("session-a")
            .join("incoming");
        std::fs::create_dir_all(&incoming).unwrap();
        std::fs::File::create(incoming.join("retained.part"))
            .unwrap()
            .set_len(RETAINED_SOURCE_BYTES_CAP)
            .unwrap();
        let error = ensure_retained_import_capacity(&root).unwrap_err();
        assert!(format!("{error:#}").contains("8589934592 bytes"));
        let _ = std::fs::remove_dir_all(root);
    }

    fn temp_root(label: &str) -> PathBuf {
        let mut random = [0_u8; 8];
        getrandom::fill(&mut random).unwrap();
        let suffix = random
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        std::env::temp_dir().join(format!(
            "ytt-retained-inventory-{label}-{}-{suffix}",
            std::process::id()
        ))
    }
}
