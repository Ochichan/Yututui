//! Handle-relative creation inside a directory whose kernel identity has been pinned.
//!
//! This module intentionally has no pathname cleanup path. Once a child has been created, a
//! failure leaves that exact generation in place for explicit recovery; deleting by name after
//! losing the handle would risk removing a replacement installed by another process.

use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::{self, Write};
use std::path::{Component, Path};
use std::sync::Arc;

use super::{FileObjectId, ensure_process_mutation_allowed, file_object_id};

/// An existing directory held open by kernel handle rather than trusted by pathname after open.
#[derive(Clone, Debug)]
pub(crate) struct PinnedDir {
    inner: Arc<PinnedDirInner>,
}

#[derive(Debug)]
struct PinnedDirInner {
    handle: File,
    identity: FileObjectId,
    private_namespace: bool,
}

/// A newly-created, exclusively-owned child generation inside a [`PinnedDir`].
///
/// The child is never unlinked from `Drop`: an interrupted write is safer to retain for explicit
/// reconciliation than to remove through a pathname which may have been exchanged meanwhile.
#[derive(Debug)]
pub(crate) struct OwnedGeneration {
    parent: PinnedDir,
    file: File,
    identity: FileObjectId,
    basename: OsString,
    residence: GenerationResidence,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GenerationResidence {
    Named,
    #[cfg(windows)]
    NamedRecoverable,
    #[cfg(target_os = "linux")]
    Anonymous,
}

impl PinnedDir {
    /// Pin `trusted_root/relative` without following any component below `trusted_root`.
    ///
    /// `trusted_root` must be an absolute, existing, non-symlink/reparse directory. Its ancestors
    /// are the caller's trust boundary. Every component in `relative` is opened relative to the
    /// preceding directory handle with no-follow semantics.
    pub(crate) fn open_existing(trusted_root: &Path, relative: &Path) -> io::Result<Self> {
        Self::open_existing_with_policy(trusted_root, relative, false)
    }

    /// Pin an app-owned private directory chain suitable for identity-checked cleanup.
    ///
    /// Unix requires every opened component, including `trusted_root`, to be owned by the
    /// effective uid and mode 0700. Windows retains the app-data writer-lease trust boundary and
    /// rejects every reparse component. Never use this constructor for a user-selected output
    /// directory.
    pub(crate) fn open_private_existing(trusted_root: &Path, relative: &Path) -> io::Result<Self> {
        Self::open_existing_with_policy(trusted_root, relative, true)
    }

    fn open_existing_with_policy(
        trusted_root: &Path,
        relative: &Path,
        private_namespace: bool,
    ) -> io::Result<Self> {
        if !trusted_root.is_absolute() {
            return Err(invalid_input("trusted root must be an absolute path"));
        }
        if relative.is_absolute() {
            return Err(invalid_input("pinned directory path must be relative"));
        }

        let mut handle = platform::open_root(trusted_root)?;
        ensure_directory_handle(&handle)?;
        if private_namespace {
            ensure_private_directory_handle(&handle)?;
        }

        for component in relative.components() {
            match component {
                Component::Normal(name) => {
                    handle = platform::open_directory_at(&handle, name)?;
                    ensure_directory_handle(&handle)?;
                    if private_namespace {
                        ensure_private_directory_handle(&handle)?;
                    }
                }
                Component::CurDir => {}
                Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                    return Err(invalid_input(
                        "pinned directory path may contain only normal relative components",
                    ));
                }
            }
        }

        let identity = file_object_id(&handle)?;
        Ok(Self {
            inner: Arc::new(PinnedDirInner {
                handle,
                identity,
                private_namespace,
            }),
        })
    }

    /// Return the stable object identity captured from the final directory handle.
    pub(crate) fn identity(&self) -> FileObjectId {
        self.inner.identity
    }

    /// Recheck that the held handle still names the pinned non-reparse directory object.
    pub(crate) fn verify(&self) -> io::Result<()> {
        ensure_directory_handle(&self.inner.handle)?;
        verify_identity(
            &self.inner.handle,
            self.inner.identity,
            "pinned directory handle changed identity",
        )
    }

    /// Create one new regular child by a single basename, without replacing any existing name.
    pub(crate) fn create_new(&self, basename: &OsStr) -> io::Result<OwnedGeneration> {
        ensure_process_mutation_allowed()?;
        validate_basename(basename)?;
        self.verify()?;

        let file = platform::create_new_file_at(&self.inner.handle, basename)?;
        ensure_regular_handle(&file)?;
        let identity = file_object_id(&file)?;

        Ok(OwnedGeneration {
            parent: self.clone(),
            file,
            identity,
            basename: basename.to_owned(),
            residence: GenerationResidence::Named,
        })
    }

    /// Create a crash-cleaned destination generation for copy-then-promote publication.
    ///
    /// Linux prefers `O_TMPFILE`; Windows and other Unix filesystems use an exclusive recoverable
    /// name. Callers must promote or retain any named fallback under their durable transaction
    /// before dropping it.
    pub(crate) fn create_ephemeral(
        &self,
        fallback_basename: &OsStr,
    ) -> io::Result<OwnedGeneration> {
        self.create_ephemeral_with(fallback_basename, platform::create_ephemeral_file_at)
    }

    fn create_ephemeral_with(
        &self,
        fallback_basename: &OsStr,
        create_ephemeral: impl FnOnce(&File, &OsStr) -> io::Result<(File, GenerationResidence)>,
    ) -> io::Result<OwnedGeneration> {
        ensure_process_mutation_allowed()?;
        validate_basename(fallback_basename)?;
        self.verify()?;
        let (file, residence) = match create_ephemeral(&self.inner.handle, fallback_basename) {
            Ok(ephemeral) => ephemeral,
            Err(error)
                if error.kind() == io::ErrorKind::Unsupported && self.inner.private_namespace =>
            {
                (
                    platform::create_new_file_at(&self.inner.handle, fallback_basename)?,
                    GenerationResidence::Named,
                )
            }
            Err(error) => return Err(error),
        };
        ensure_regular_handle(&file)?;
        let identity = file_object_id(&file)?;
        Ok(OwnedGeneration {
            parent: self.clone(),
            file,
            identity,
            basename: fallback_basename.to_owned(),
            residence,
        })
    }

    /// Open one existing regular child relative to the pinned directory and capture its identity.
    pub(crate) fn open_child(&self, basename: &OsStr) -> io::Result<OwnedGeneration> {
        validate_basename(basename)?;
        self.verify()?;
        let file = platform::open_file_at(&self.inner.handle, basename)?;
        ensure_regular_handle(&file)?;
        let identity = file_object_id(&file)?;
        Ok(OwnedGeneration {
            parent: self.clone(),
            file,
            identity,
            basename: basename.to_owned(),
            residence: GenerationResidence::Named,
        })
    }

    /// Open one existing regular child with read-only kernel access.
    ///
    /// This is the source-copy/readback path. In particular, it does not require write or delete
    /// access on Windows and therefore also works for user-provided read-only artifacts.
    pub(crate) fn open_child_readonly(&self, basename: &OsStr) -> io::Result<OwnedGeneration> {
        validate_basename(basename)?;
        self.verify()?;
        let file = platform::open_file_at_readonly(&self.inner.handle, basename)?;
        ensure_regular_handle(&file)?;
        let identity = file_object_id(&file)?;
        Ok(OwnedGeneration {
            parent: self.clone(),
            file,
            identity,
            basename: basename.to_owned(),
            residence: GenerationResidence::Named,
        })
    }

    /// Open one existing child only if it is the exact persisted kernel object.
    pub(crate) fn open_existing_child(
        &self,
        basename: &OsStr,
        expected: FileObjectId,
    ) -> io::Result<OwnedGeneration> {
        let generation = self.open_child(basename)?;
        if generation.identity == expected {
            Ok(generation)
        } else {
            Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "existing child does not match its persisted object identity",
            ))
        }
    }

    /// Reopen a journal-owned Windows stage while preserving its recoverable residence marker.
    #[cfg(windows)]
    pub(crate) fn open_existing_recoverable_child(
        &self,
        basename: &OsStr,
        expected: FileObjectId,
    ) -> io::Result<OwnedGeneration> {
        let mut generation = self.open_existing_child(basename, expected)?;
        generation.residence = GenerationResidence::NamedRecoverable;
        Ok(generation)
    }

    /// Open one existing child read-only only if it is the exact persisted kernel object.
    pub(crate) fn open_existing_child_readonly(
        &self,
        basename: &OsStr,
        expected: FileObjectId,
    ) -> io::Result<OwnedGeneration> {
        let generation = self.open_child_readonly(basename)?;
        if generation.identity == expected {
            Ok(generation)
        } else {
            Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "existing child does not match its persisted object identity",
            ))
        }
    }

    /// Durably flush directory metadata through the pinned directory handle.
    pub(crate) fn sync_directory(&self) -> io::Result<()> {
        ensure_process_mutation_allowed()?;
        self.verify()?;
        platform::sync_directory(&self.inner.handle)?;
        self.verify()
    }

    /// Try to lock one exact child without reopening it by pathname.
    pub(crate) fn try_lock_child(
        &self,
        basename: &OsStr,
        expected: Option<FileObjectId>,
        create_if_missing: bool,
    ) -> io::Result<Option<(super::AdvisoryFileLock, FileObjectId)>> {
        validate_basename(basename)?;
        self.verify()?;
        let generation = match expected {
            Some(expected) => self.open_existing_child(basename, expected)?,
            None => match self.open_child(basename) {
                Ok(generation) => generation,
                Err(error) if create_if_missing && error.kind() == io::ErrorKind::NotFound => {
                    match self.create_new(basename) {
                        Ok(generation) => generation,
                        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                            self.open_child(basename)?
                        }
                        Err(error) => return Err(error),
                    }
                }
                Err(error) => return Err(error),
            },
        };
        generation.verify()?;
        let identity = generation.identity;
        if super::try_lock_file(&generation.file)? {
            Ok(Some((
                super::AdvisoryFileLock {
                    _file: generation.file,
                },
                identity,
            )))
        } else {
            Ok(None)
        }
    }
}

impl OwnedGeneration {
    /// Borrow the already-open child for handle-only reads.
    pub(crate) fn file(&self) -> io::Result<&File> {
        self.verify()?;
        Ok(&self.file)
    }

    /// Borrow the already-open, exclusively-created file handle for writing.
    ///
    /// Callers should keep this borrow short and call [`Self::sync_durable`] before treating the
    /// generation as published. The mutation gate is checked both here and at the durability
    /// boundary.
    pub(crate) fn file_mut(&mut self) -> io::Result<&mut File> {
        ensure_process_mutation_allowed()?;
        self.verify()?;
        Ok(&mut self.file)
    }

    /// The single directory-entry name exclusively created for this generation.
    pub(crate) fn basename(&self) -> &OsStr {
        &self.basename
    }

    /// Stable identity of the open child file.
    pub(crate) fn identity(&self) -> FileObjectId {
        self.identity
    }

    /// Set Unix permission bits on this exact owned generation, without resolving its name.
    #[cfg(unix)]
    pub(crate) fn set_mode(&mut self, mode: u32) -> io::Result<()> {
        use std::os::unix::fs::PermissionsExt as _;

        ensure_process_mutation_allowed()?;
        self.verify()?;
        self.file
            .set_permissions(std::fs::Permissions::from_mode(mode & 0o777))?;
        self.verify()
    }

    /// Whether dropping this handle before promotion leaves a named recovery generation.
    pub(crate) fn retains_name_on_drop(&self) -> bool {
        match self.residence {
            GenerationResidence::Named => true,
            #[cfg(windows)]
            GenerationResidence::NamedRecoverable => true,
            #[cfg(target_os = "linux")]
            GenerationResidence::Anonymous => false,
        }
    }

    /// Publish this open generation under a new name in `destination` without replacing a name.
    ///
    /// Linux creates a hard link from the open file object; macOS uses `fclonefileat`, whose
    /// source is likewise the open handle. Unsupported filesystems fail closed. The owned source
    /// generation is intentionally retained.
    #[cfg(not(windows))]
    pub(crate) fn publish_noreplace(
        &self,
        destination: &PinnedDir,
        basename: &OsStr,
    ) -> io::Result<OwnedGeneration> {
        ensure_process_mutation_allowed()?;
        validate_basename(basename)?;
        self.parent.verify()?;
        destination.verify()?;
        self.verify()?;
        platform::publish_file_at(&self.file, &destination.inner.handle, basename)?;
        destination.sync_directory()?;
        destination.open_child_readonly(basename)
    }

    /// Promote an owned copy into `destination` without replacement and without a successful
    /// stage entry remaining behind.
    ///
    /// Anonymous generations are linked from their exact handle. Named fallbacks are moved with
    /// the platform's no-replace primitive; Windows binds the rename directly to this file
    /// handle. The returned handle is reopened read-only at the final name.
    pub(crate) fn promote_noreplace(
        self,
        destination: &PinnedDir,
        basename: &OsStr,
    ) -> io::Result<OwnedGeneration> {
        ensure_process_mutation_allowed()?;
        validate_basename(basename)?;
        self.parent.verify()?;
        destination.verify()?;
        self.verify()?;
        self.file.sync_all()?;

        if self.residence == GenerationResidence::Named && !self.parent.inner.private_namespace {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "named no-replace promotion requires an app-owned private source namespace",
            ));
        }

        let source_is_named = match self.residence {
            GenerationResidence::Named => true,
            #[cfg(windows)]
            GenerationResidence::NamedRecoverable => true,
            #[cfg(target_os = "linux")]
            GenerationResidence::Anonymous => false,
        };
        if self.residence == GenerationResidence::Named {
            match self.parent.open_child_readonly(&self.basename) {
                Ok(current) if current.identity() == self.identity => {}
                Ok(_) => {
                    return Err(io::Error::new(
                        io::ErrorKind::AlreadyExists,
                        "owned stage name was replaced before promotion",
                    ));
                }
                Err(error) => return Err(error),
            }
        }
        platform::promote_file_at(
            &self.file,
            &self.parent.inner.handle,
            &self.basename,
            source_is_named,
            &destination.inner.handle,
            basename,
        )?;
        self.parent.sync_directory()?;
        destination.sync_directory()?;
        let published = destination.open_child_readonly(basename)?;
        if published.identity() != self.identity {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "promoted destination does not match its owned stage identity",
            ));
        }
        Ok(published)
    }

    /// Remove this exact generation from an app-owned private parent.
    ///
    /// POSIX has no conditional unlink-by-handle operation. Safety therefore relies on the
    /// private 0700 namespace and its process writer/owner lease being the only writer. The exact
    /// object is reopened and compared immediately before handle-relative unlink; Windows marks
    /// the already-open object itself for deletion. User-selected destination directories can
    /// never call this API because they cannot construct a private `PinnedDir`.
    pub(crate) fn remove_from_private_parent(self) -> io::Result<()> {
        ensure_process_mutation_allowed()?;
        if !self.parent.inner.private_namespace {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "exact-generation cleanup requires a pinned private namespace",
            ));
        }
        if self.residence != GenerationResidence::Named {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "anonymous generation has no private directory entry to remove",
            ));
        }
        self.parent.verify()?;
        self.verify()?;
        let removal_file =
            platform::open_file_at_delete(&self.parent.inner.handle, &self.basename)?;
        ensure_regular_handle(&removal_file)?;
        if file_object_id(&removal_file)? != self.identity {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "private cleanup name no longer matches the owned generation",
            ));
        }
        platform::remove_file_at(&removal_file, &self.parent.inner.handle, &self.basename)?;
        let parent = self.parent.clone();
        let basename = self.basename.clone();
        drop(removal_file);
        drop(self.file);
        parent.sync_directory()?;
        match parent.open_child_readonly(&basename) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Ok(_) => Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "private cleanup name was repopulated after exact removal",
            )),
            Err(error) => Err(error),
        }
    }

    /// Flush file contents and metadata, then flush the containing pinned directory handle.
    pub(crate) fn sync_durable(&mut self) -> io::Result<()> {
        ensure_process_mutation_allowed()?;
        self.parent.verify()?;
        self.verify()?;
        self.file.flush()?;
        self.file.sync_all()?;
        self.verify()?;
        self.parent.sync_directory()
    }

    fn verify(&self) -> io::Result<()> {
        ensure_regular_handle(&self.file)?;
        verify_identity(
            &self.file,
            self.identity,
            "owned child handle changed identity",
        )
    }
}

fn validate_basename(basename: &OsStr) -> io::Result<()> {
    let mut components = Path::new(basename).components();
    match (components.next(), components.next()) {
        (Some(Component::Normal(name)), None) if name == basename => {
            platform::validate_native_basename(basename)
        }
        _ => Err(invalid_input(
            "child name must be exactly one non-empty normal basename",
        )),
    }
}

fn verify_identity(file: &File, expected: FileObjectId, message: &str) -> io::Result<()> {
    let observed = file_object_id(file)?;
    if observed == expected {
        Ok(())
    } else {
        Err(io::Error::new(io::ErrorKind::PermissionDenied, message))
    }
}

fn invalid_input(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

#[cfg(unix)]
fn ensure_directory_handle(file: &File) -> io::Result<()> {
    if file.metadata()?.is_dir() {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::NotADirectory,
            "pinned handle is not a directory",
        ))
    }
}

#[cfg(windows)]
fn ensure_directory_handle(file: &File) -> io::Result<()> {
    use std::os::windows::fs::MetadataExt;
    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;

    let metadata = file.metadata()?;
    if metadata.is_dir() && metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT == 0 {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::NotADirectory,
            "pinned handle is not a non-reparse directory",
        ))
    }
}

#[cfg(not(any(unix, windows)))]
fn ensure_directory_handle(_file: &File) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "pinned directories are unsupported on this platform",
    ))
}

#[cfg(unix)]
fn ensure_private_directory_handle(file: &File) -> io::Result<()> {
    use std::os::unix::fs::MetadataExt;

    let metadata = file.metadata()?;
    // SAFETY: geteuid has no preconditions and only reads process credentials.
    let effective_uid = unsafe { libc::geteuid() };
    if metadata.is_dir() && metadata.uid() == effective_uid && metadata.mode() & 0o777 == 0o700 {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "private pinned directory must be owned by the effective uid with mode 0700",
        ))
    }
}

#[cfg(windows)]
fn ensure_private_directory_handle(file: &File) -> io::Result<()> {
    // ACL ownership is established by the app-data writer lease. At this layer, retaining an
    // exact non-reparse directory handle is the enforceable Windows trust boundary.
    ensure_directory_handle(file)
}

#[cfg(not(any(unix, windows)))]
fn ensure_private_directory_handle(_file: &File) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "private pinned directories are unsupported on this platform",
    ))
}

#[cfg(unix)]
fn ensure_regular_handle(file: &File) -> io::Result<()> {
    if file.metadata()?.is_file() {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "owned generation handle is not a regular file",
        ))
    }
}

#[cfg(windows)]
fn ensure_regular_handle(file: &File) -> io::Result<()> {
    use std::os::windows::fs::MetadataExt;
    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;

    let metadata = file.metadata()?;
    if metadata.is_file() && metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT == 0 {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "owned generation handle is not a non-reparse regular file",
        ))
    }
}

#[cfg(not(any(unix, windows)))]
fn ensure_regular_handle(_file: &File) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "owned generations are unsupported on this platform",
    ))
}

#[cfg(unix)]
mod platform {
    use std::ffi::{CString, OsStr};
    use std::fs::File;
    use std::io;
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::ffi::OsStrExt;
    use std::path::Path;

    pub(super) fn open_root(path: &Path) -> io::Result<File> {
        let encoded = CString::new(path.as_os_str().as_bytes())
            .map_err(|_| super::invalid_input("trusted root contains a NUL byte"))?;
        // SAFETY: `encoded` is NUL-terminated, flags request an existing directory, and the
        // returned descriptor is immediately transferred into exactly one `File` owner.
        let fd = unsafe {
            libc::open(
                encoded.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        owned_fd(fd)
    }

    pub(super) fn open_directory_at(parent: &File, name: &OsStr) -> io::Result<File> {
        let encoded = component_cstring(name)?;
        // SAFETY: `parent` owns a live directory descriptor and `encoded` is a single,
        // NUL-terminated component. O_NOFOLLOW rejects a symlink in the opened component.
        let fd = unsafe {
            libc::openat(
                parent.as_raw_fd(),
                encoded.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        owned_fd(fd)
    }

    pub(super) fn create_new_file_at(parent: &File, name: &OsStr) -> io::Result<File> {
        let encoded = component_cstring(name)?;
        // SAFETY: `parent` owns a live directory descriptor and `encoded` is a single,
        // NUL-terminated component. O_EXCL is the no-replace publication boundary, while
        // O_NOFOLLOW prevents a pre-existing symlink from being opened.
        let fd = unsafe {
            libc::openat(
                parent.as_raw_fd(),
                encoded.as_ptr(),
                libc::O_RDWR | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                0o600,
            )
        };
        owned_fd(fd)
    }

    #[cfg(target_os = "linux")]
    pub(super) fn create_ephemeral_file_at(
        parent: &File,
        name: &OsStr,
    ) -> io::Result<(File, super::GenerationResidence)> {
        // SAFETY: `parent` is a live directory descriptor. `.` selects that exact directory and
        // O_TMPFILE creates an unnamed inode which is reclaimed automatically on interruption.
        let fd = unsafe {
            libc::openat(
                parent.as_raw_fd(),
                c".".as_ptr(),
                libc::O_TMPFILE | libc::O_RDWR | libc::O_CLOEXEC,
                0o600,
            )
        };
        if fd >= 0 {
            return owned_fd(fd).map(|file| (file, super::GenerationResidence::Anonymous));
        }
        let error = io::Error::last_os_error();
        if matches!(
            error.raw_os_error(),
            Some(libc::EOPNOTSUPP | libc::EINVAL | libc::EISDIR | libc::ENOENT)
        ) {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!(
                    "destination filesystem does not support anonymous no-replace staging for {}",
                    Path::new(name).display()
                ),
            ));
        }
        Err(error)
    }

    #[cfg(not(target_os = "linux"))]
    pub(super) fn create_ephemeral_file_at(
        _parent: &File,
        name: &OsStr,
    ) -> io::Result<(File, super::GenerationResidence)> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            format!(
                "destination filesystem does not support exact-handle clone publication for {}",
                Path::new(name).display()
            ),
        ))
    }

    pub(super) fn open_file_at(parent: &File, name: &OsStr) -> io::Result<File> {
        let encoded = component_cstring(name)?;
        // SAFETY: the parent descriptor and single-component C string remain live through the
        // call; O_NOFOLLOW rejects a final symlink and the returned fd has one File owner.
        let fd = unsafe {
            libc::openat(
                parent.as_raw_fd(),
                encoded.as_ptr(),
                libc::O_RDWR | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        owned_fd(fd)
    }

    pub(super) fn open_file_at_readonly(parent: &File, name: &OsStr) -> io::Result<File> {
        let encoded = component_cstring(name)?;
        // SAFETY: the parent descriptor and single-component C string remain live through the
        // call; O_NOFOLLOW rejects a final symlink and O_RDONLY does not require source write
        // permission.
        let fd = unsafe {
            libc::openat(
                parent.as_raw_fd(),
                encoded.as_ptr(),
                libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        owned_fd(fd)
    }

    pub(super) fn open_file_at_delete(parent: &File, name: &OsStr) -> io::Result<File> {
        open_file_at_readonly(parent, name)
    }

    pub(super) fn remove_file_at(_exact: &File, parent: &File, name: &OsStr) -> io::Result<()> {
        let encoded = component_cstring(name)?;
        // SAFETY: the caller has pinned and verified the private parent and exact child under its
        // sole-writer lease immediately before this handle-relative unlink.
        let result = unsafe { libc::unlinkat(parent.as_raw_fd(), encoded.as_ptr(), 0) };
        if result == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }

    #[cfg(target_os = "linux")]
    pub(super) fn publish_file_at(
        source: &File,
        destination: &File,
        name: &OsStr,
    ) -> io::Result<()> {
        let encoded = component_cstring(name)?;
        let empty = c"";
        // SAFETY: both descriptors are live, the destination is a pinned directory, and
        // AT_EMPTY_PATH binds the hard link to `source` itself rather than a mutable source name.
        let result = unsafe {
            libc::linkat(
                source.as_raw_fd(),
                empty.as_ptr(),
                destination.as_raw_fd(),
                encoded.as_ptr(),
                libc::AT_EMPTY_PATH,
            )
        };
        if result == 0 {
            return Ok(());
        }
        let first = io::Error::last_os_error();
        if !matches!(first.raw_os_error(), Some(libc::EPERM | libc::ENOENT)) {
            return Err(first);
        }

        // Some Linux policies deny AT_EMPTY_PATH without CAP_DAC_READ_SEARCH. `/proc/self/fd`
        // still names this exact open handle; AT_SYMLINK_FOLLOW dereferences that procfs handle,
        // never a caller-controlled source pathname.
        let proc_path = CString::new(format!("/proc/self/fd/{}", source.as_raw_fd()))
            .expect("decimal file descriptor contains no NUL");
        // SAFETY: proc_path and encoded are live C strings and both descriptors remain open.
        let result = unsafe {
            libc::linkat(
                libc::AT_FDCWD,
                proc_path.as_ptr(),
                destination.as_raw_fd(),
                encoded.as_ptr(),
                libc::AT_SYMLINK_FOLLOW,
            )
        };
        if result == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }

    #[cfg(target_os = "macos")]
    pub(super) fn publish_file_at(
        source: &File,
        destination: &File,
        name: &OsStr,
    ) -> io::Result<()> {
        let encoded = component_cstring(name)?;
        // SAFETY: the source and destination directory descriptors are live and `encoded` is a
        // single NUL-terminated destination component. flags=0 forbids replacement.
        let result = unsafe {
            libc::fclonefileat(
                source.as_raw_fd(),
                destination.as_raw_fd(),
                encoded.as_ptr(),
                0,
            )
        };
        if result == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    pub(super) fn publish_file_at(
        _source: &File,
        _destination: &File,
        _name: &OsStr,
    ) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "handle-bound no-replace publication is unsupported on this Unix platform",
        ))
    }

    #[cfg(target_os = "linux")]
    pub(super) fn promote_file_at(
        source: &File,
        source_parent: &File,
        source_name: &OsStr,
        source_is_named: bool,
        destination: &File,
        destination_name: &OsStr,
    ) -> io::Result<()> {
        if !source_is_named {
            return publish_file_at(source, destination, destination_name);
        }
        let source_name = component_cstring(source_name)?;
        let destination_name = component_cstring(destination_name)?;
        // SAFETY: both pinned directory descriptors and component strings remain live for this
        // atomic no-replace move. The caller holds the private writer lease for the named
        // fallback and verifies its exact identity immediately before this call.
        let result = unsafe {
            libc::renameat2(
                source_parent.as_raw_fd(),
                source_name.as_ptr(),
                destination.as_raw_fd(),
                destination_name.as_ptr(),
                libc::RENAME_NOREPLACE,
            )
        };
        if result == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }

    #[cfg(target_os = "macos")]
    pub(super) fn promote_file_at(
        source: &File,
        source_parent: &File,
        source_name: &OsStr,
        source_is_named: bool,
        destination: &File,
        destination_name: &OsStr,
    ) -> io::Result<()> {
        if !source_is_named {
            return publish_file_at(source, destination, destination_name);
        }
        let source_name = component_cstring(source_name)?;
        let destination_name = component_cstring(destination_name)?;
        // SAFETY: both pinned directory descriptors and component strings remain live. The
        // private writer lease protects the named fallback; RENAME_EXCL forbids replacement.
        let result = unsafe {
            libc::renameatx_np(
                source_parent.as_raw_fd(),
                source_name.as_ptr(),
                destination.as_raw_fd(),
                destination_name.as_ptr(),
                libc::RENAME_EXCL,
            )
        };
        if result == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    pub(super) fn promote_file_at(
        _source: &File,
        _source_parent: &File,
        _source_name: &OsStr,
        _source_is_named: bool,
        _destination: &File,
        _destination_name: &OsStr,
    ) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "no-replace owned-generation promotion is unsupported on this Unix platform",
        ))
    }

    pub(super) fn sync_directory(directory: &File) -> io::Result<()> {
        directory.sync_all()
    }

    pub(super) fn validate_native_basename(name: &OsStr) -> io::Result<()> {
        component_cstring(name).map(|_| ())
    }

    fn component_cstring(name: &OsStr) -> io::Result<CString> {
        CString::new(name.as_bytes())
            .map_err(|_| super::invalid_input("child name contains a NUL byte"))
    }

    fn owned_fd(fd: libc::c_int) -> io::Result<File> {
        if fd < 0 {
            Err(io::Error::last_os_error())
        } else {
            // SAFETY: successful open/openat returned a fresh owned descriptor.
            Ok(unsafe { File::from_raw_fd(fd) })
        }
    }
}

#[cfg(windows)]
mod platform {
    use std::ffi::OsStr;
    use std::fs::File;
    use std::io;
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::{AsRawHandle, FromRawHandle};
    use std::path::Path;
    use std::ptr;

    use windows_sys::Wdk::Foundation::OBJECT_ATTRIBUTES;
    use windows_sys::Wdk::Storage::FileSystem::{
        FILE_CREATE, FILE_DIRECTORY_FILE, FILE_DISPOSITION_DELETE,
        FILE_DISPOSITION_IGNORE_READONLY_ATTRIBUTE, FILE_DISPOSITION_INFORMATION,
        FILE_DISPOSITION_INFORMATION_EX, FILE_DISPOSITION_POSIX_SEMANTICS, FILE_NON_DIRECTORY_FILE,
        FILE_OPEN, FILE_OPEN_REPARSE_POINT, FILE_RENAME_INFORMATION, FILE_SYNCHRONOUS_IO_NONALERT,
        FILE_WRITE_THROUGH, FileDispositionInformation, FileDispositionInformationEx,
        FileRenameInformation, FileRenameInformationEx, NtCreateFile, NtFlushBuffersFileEx,
        NtSetInformationFile,
    };
    use windows_sys::Win32::Foundation::{
        HANDLE, INVALID_HANDLE_VALUE, OBJ_CASE_INSENSITIVE, RtlNtStatusToDosError,
        STATUS_INVALID_INFO_CLASS, STATUS_INVALID_PARAMETER, STATUS_NOT_SUPPORTED, UNICODE_STRING,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, DELETE, FILE_ADD_FILE, FILE_ADD_SUBDIRECTORY, FILE_ATTRIBUTE_NORMAL,
        FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_FLAG_WRITE_THROUGH,
        FILE_GENERIC_READ, FILE_GENERIC_WRITE, FILE_LIST_DIRECTORY, FILE_READ_ATTRIBUTES,
        FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING, SYNCHRONIZE,
    };
    use windows_sys::Win32::System::IO::IO_STATUS_BLOCK;

    // NtFlushBuffersFileEx requires write or append access. For a directory those rights map to
    // FILE_ADD_FILE and FILE_ADD_SUBDIRECTORY respectively; request both so metadata flushes are
    // authorized on NTFS/ReFS instead of failing every durable publication with ACCESS_DENIED.
    const DIRECTORY_ACCESS: u32 = FILE_LIST_DIRECTORY
        | FILE_ADD_FILE
        | FILE_ADD_SUBDIRECTORY
        | FILE_READ_ATTRIBUTES
        | SYNCHRONIZE;

    pub(super) fn open_root(path: &Path) -> io::Result<File> {
        let wide = nul_terminated(path.as_os_str())?;
        // SAFETY: `wide` is NUL-terminated for the duration of the call. The returned handle is
        // checked before ownership is transferred to `File`; null security/template pointers are
        // accepted by CreateFileW.
        let handle = unsafe {
            CreateFileW(
                wide.as_ptr(),
                DIRECTORY_ACCESS,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                ptr::null(),
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_WRITE_THROUGH,
                ptr::null_mut(),
            )
        };
        owned_handle(handle)
    }

    pub(super) fn open_directory_at(parent: &File, name: &OsStr) -> io::Result<File> {
        nt_open_relative(
            parent,
            name,
            DIRECTORY_ACCESS,
            FILE_OPEN,
            FILE_DIRECTORY_FILE
                | FILE_OPEN_REPARSE_POINT
                | FILE_SYNCHRONOUS_IO_NONALERT
                | FILE_WRITE_THROUGH,
        )
    }

    pub(super) fn create_new_file_at(parent: &File, name: &OsStr) -> io::Result<File> {
        nt_open_relative(
            parent,
            name,
            FILE_GENERIC_READ | FILE_GENERIC_WRITE | DELETE | SYNCHRONIZE,
            FILE_CREATE,
            FILE_NON_DIRECTORY_FILE
                | FILE_OPEN_REPARSE_POINT
                | FILE_SYNCHRONOUS_IO_NONALERT
                | FILE_WRITE_THROUGH,
        )
    }

    pub(super) fn create_ephemeral_file_at(
        parent: &File,
        name: &OsStr,
    ) -> io::Result<(File, super::GenerationResidence)> {
        // Clearing a delete-on-close disposition after an exact-handle rename can return
        // ACCESS_DENIED on Windows even though the rename succeeded. This namespace is private
        // and the durable transaction journals the exclusive fallback name, so retain it for
        // explicit recovery instead.
        create_new_file_at(parent, name)
            .map(|file| (file, super::GenerationResidence::NamedRecoverable))
    }

    pub(super) fn open_file_at(parent: &File, name: &OsStr) -> io::Result<File> {
        nt_open_relative(
            parent,
            name,
            FILE_GENERIC_READ | FILE_GENERIC_WRITE | DELETE | SYNCHRONIZE,
            FILE_OPEN,
            FILE_NON_DIRECTORY_FILE
                | FILE_OPEN_REPARSE_POINT
                | FILE_SYNCHRONOUS_IO_NONALERT
                | FILE_WRITE_THROUGH,
        )
    }

    pub(super) fn open_file_at_readonly(parent: &File, name: &OsStr) -> io::Result<File> {
        nt_open_relative(
            parent,
            name,
            FILE_GENERIC_READ | FILE_READ_ATTRIBUTES | SYNCHRONIZE,
            FILE_OPEN,
            FILE_NON_DIRECTORY_FILE | FILE_OPEN_REPARSE_POINT | FILE_SYNCHRONOUS_IO_NONALERT,
        )
    }

    pub(super) fn open_file_at_delete(parent: &File, name: &OsStr) -> io::Result<File> {
        nt_open_relative(
            parent,
            name,
            FILE_GENERIC_READ | DELETE | SYNCHRONIZE,
            FILE_OPEN,
            FILE_NON_DIRECTORY_FILE | FILE_OPEN_REPARSE_POINT | FILE_SYNCHRONOUS_IO_NONALERT,
        )
    }

    pub(super) fn remove_file_at(exact: &File, _parent: &File, _name: &OsStr) -> io::Result<()> {
        let mut disposition = FILE_DISPOSITION_INFORMATION_EX {
            Flags: FILE_DISPOSITION_DELETE
                | FILE_DISPOSITION_POSIX_SEMANTICS
                | FILE_DISPOSITION_IGNORE_READONLY_ATTRIBUTE,
        };
        let mut io_status = IO_STATUS_BLOCK::default();
        // SAFETY: `exact` is the identity-verified child handle with DELETE access and the
        // disposition buffer remains live for the synchronous operation.
        let mut status = unsafe {
            NtSetInformationFile(
                exact.as_raw_handle() as HANDLE,
                &mut io_status,
                ptr::addr_of_mut!(disposition).cast(),
                u32::try_from(std::mem::size_of::<FILE_DISPOSITION_INFORMATION_EX>())
                    .expect("extended disposition size fits u32"),
                FileDispositionInformationEx,
            )
        };
        if matches!(
            status,
            STATUS_INVALID_INFO_CLASS | STATUS_INVALID_PARAMETER | STATUS_NOT_SUPPORTED
        ) {
            let mut legacy = FILE_DISPOSITION_INFORMATION { DeleteFile: true };
            io_status = IO_STATUS_BLOCK::default();
            // SAFETY: same exact DELETE-capable handle with the legacy fixed-size buffer.
            status = unsafe {
                NtSetInformationFile(
                    exact.as_raw_handle() as HANDLE,
                    &mut io_status,
                    ptr::addr_of_mut!(legacy).cast(),
                    u32::try_from(std::mem::size_of::<FILE_DISPOSITION_INFORMATION>())
                        .expect("legacy disposition size fits u32"),
                    FileDispositionInformation,
                )
            };
        }
        nt_result(status)
    }

    pub(super) fn promote_file_at(
        source: &File,
        _source_parent: &File,
        _source_name: &OsStr,
        source_is_named: bool,
        destination: &File,
        name: &OsStr,
    ) -> io::Result<()> {
        if !source_is_named {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "Windows owned generation unexpectedly has no stage name",
            ));
        }
        let wide: Vec<u16> = name.encode_wide().collect();
        let name_bytes = u32::from(unicode_length(&wide)?);
        let offset = std::mem::offset_of!(FILE_RENAME_INFORMATION, FileName);
        let byte_len = offset
            .checked_add(wide.len() * std::mem::size_of::<u16>())
            .ok_or_else(|| super::invalid_input("Windows rename information is too long"))?;
        let words = byte_len.div_ceil(std::mem::size_of::<usize>());
        let mut storage = vec![0_usize; words];
        let information = storage.as_mut_ptr().cast::<FILE_RENAME_INFORMATION>();
        // SAFETY: storage is aligned and sized for the rename header and UTF-16 destination.
        // Flags zero forbids replacement for both the extended and legacy information classes.
        unsafe {
            (*information).Anonymous.Flags = 0;
            (*information).RootDirectory = destination.as_raw_handle() as HANDLE;
            (*information).FileNameLength = name_bytes;
            ptr::copy_nonoverlapping(
                wide.as_ptr(),
                ptr::addr_of_mut!((*information).FileName).cast::<u16>(),
                wide.len(),
            );
        }
        let mut io_status = IO_STATUS_BLOCK::default();
        // SAFETY: the source handle has DELETE access and all buffers/handles remain live for the
        // synchronous exact-handle rename.
        let mut status = unsafe {
            NtSetInformationFile(
                source.as_raw_handle() as HANDLE,
                &mut io_status,
                information.cast(),
                u32::try_from(byte_len).expect("bounded rename information fits u32"),
                FileRenameInformationEx,
            )
        };
        if matches!(
            status,
            STATUS_INVALID_INFO_CLASS | STATUS_INVALID_PARAMETER | STATUS_NOT_SUPPORTED
        ) {
            io_status = IO_STATUS_BLOCK::default();
            // SAFETY: same validated buffer as above; the legacy class also treats false/zero as
            // no replacement and provides the compatibility fallback for older filesystems.
            status = unsafe {
                NtSetInformationFile(
                    source.as_raw_handle() as HANDLE,
                    &mut io_status,
                    information.cast(),
                    u32::try_from(byte_len).expect("bounded rename information fits u32"),
                    FileRenameInformation,
                )
            };
        }
        if matches!(
            status,
            STATUS_INVALID_INFO_CLASS | STATUS_INVALID_PARAMETER | STATUS_NOT_SUPPORTED
        ) {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "filesystem does not support exact-handle no-replace rename",
            ));
        }
        nt_result(status)?;

        let mut disposition = FILE_DISPOSITION_INFORMATION { DeleteFile: false };
        io_status = IO_STATUS_BLOCK::default();
        // SAFETY: clearing the create-time delete-on-close disposition keeps the promoted final
        // name alive. If this fails, returning the error lets handle close remove the incomplete
        // publication and the durable transaction retry from its retained source.
        let status = unsafe {
            NtSetInformationFile(
                source.as_raw_handle() as HANDLE,
                &mut io_status,
                ptr::addr_of_mut!(disposition).cast(),
                u32::try_from(std::mem::size_of::<FILE_DISPOSITION_INFORMATION>())
                    .expect("disposition information size fits u32"),
                FileDispositionInformation,
            )
        };
        nt_result(status)
    }

    pub(super) fn sync_directory(directory: &File) -> io::Result<()> {
        let mut io_status = IO_STATUS_BLOCK::default();
        // SAFETY: `directory` owns a live handle with FILE_ADD_FILE/SYNCHRONIZE access and the
        // output block remains writable for the duration of this synchronous call. Flags zero
        // request the documented full data/metadata flush without a parameter buffer.
        let status = unsafe {
            NtFlushBuffersFileEx(
                directory.as_raw_handle() as HANDLE,
                0,
                ptr::null(),
                0,
                &mut io_status,
            )
        };
        nt_result(status)
    }

    pub(super) fn validate_native_basename(name: &OsStr) -> io::Result<()> {
        let wide: Vec<u16> = name.encode_wide().collect();
        if wide.is_empty()
            || wide.iter().any(|unit| {
                *unit == 0
                    || *unit == u16::from(b'/')
                    || *unit == u16::from(b'\\')
                    || *unit == u16::from(b':')
            })
        {
            return Err(super::invalid_input(
                "Windows child name contains an invalid separator, stream marker, or NUL",
            ));
        }
        unicode_length(&wide).map(|_| ())
    }

    fn nt_open_relative(
        parent: &File,
        name: &OsStr,
        desired_access: u32,
        disposition: u32,
        options: u32,
    ) -> io::Result<File> {
        let mut wide: Vec<u16> = name.encode_wide().collect();
        let byte_length = unicode_length(&wide)?;
        let unicode_name = UNICODE_STRING {
            Length: byte_length,
            MaximumLength: byte_length,
            Buffer: wide.as_mut_ptr(),
        };
        let attributes = OBJECT_ATTRIBUTES {
            Length: u32::try_from(std::mem::size_of::<OBJECT_ATTRIBUTES>())
                .expect("OBJECT_ATTRIBUTES size fits in u32"),
            RootDirectory: parent.as_raw_handle() as HANDLE,
            ObjectName: &unicode_name,
            Attributes: OBJ_CASE_INSENSITIVE,
            SecurityDescriptor: ptr::null(),
            SecurityQualityOfService: ptr::null(),
        };
        let mut io_status = IO_STATUS_BLOCK::default();
        let mut handle: HANDLE = ptr::null_mut();
        // SAFETY: all pointed-to buffers live through the synchronous call; RootDirectory is a
        // live pinned directory handle; disposition FILE_CREATE is used for child creation, so
        // this routine never overwrites an existing directory entry.
        let status = unsafe {
            NtCreateFile(
                &mut handle,
                desired_access,
                &attributes,
                &mut io_status,
                ptr::null(),
                FILE_ATTRIBUTE_NORMAL,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                disposition,
                options,
                ptr::null(),
                0,
            )
        };
        nt_result(status)?;
        owned_handle(handle)
    }

    fn unicode_length(wide: &[u16]) -> io::Result<u16> {
        let bytes = wide
            .len()
            .checked_mul(std::mem::size_of::<u16>())
            .and_then(|bytes| u16::try_from(bytes).ok())
            .ok_or_else(|| super::invalid_input("Windows child name is too long"))?;
        Ok(bytes)
    }

    fn nul_terminated(name: &OsStr) -> io::Result<Vec<u16>> {
        let mut wide: Vec<u16> = name.encode_wide().collect();
        if wide.contains(&0) {
            return Err(super::invalid_input("trusted root contains a NUL byte"));
        }
        wide.push(0);
        Ok(wide)
    }

    fn nt_result(status: i32) -> io::Result<()> {
        if status >= 0 {
            Ok(())
        } else {
            // SAFETY: conversion accepts any failing NTSTATUS and returns its Win32 error code.
            let code = unsafe { RtlNtStatusToDosError(status) };
            Err(io::Error::from_raw_os_error(code as i32))
        }
    }

    fn owned_handle(handle: HANDLE) -> io::Result<File> {
        if handle == INVALID_HANDLE_VALUE || handle.is_null() {
            Err(io::Error::last_os_error())
        } else {
            // SAFETY: successful CreateFileW/NtCreateFile returned a fresh owned handle.
            Ok(unsafe { File::from_raw_handle(handle) })
        }
    }
}

#[cfg(not(any(unix, windows)))]
mod platform {
    use std::ffi::OsStr;
    use std::fs::File;
    use std::io;
    use std::path::Path;

    fn unsupported() -> io::Error {
        io::Error::new(
            io::ErrorKind::Unsupported,
            "handle-relative filesystem operations are unsupported on this platform",
        )
    }

    pub(super) fn open_root(_path: &Path) -> io::Result<File> {
        Err(unsupported())
    }

    pub(super) fn open_directory_at(_parent: &File, _name: &OsStr) -> io::Result<File> {
        Err(unsupported())
    }

    pub(super) fn create_new_file_at(_parent: &File, _name: &OsStr) -> io::Result<File> {
        Err(unsupported())
    }

    pub(super) fn create_ephemeral_file_at(
        _parent: &File,
        _name: &OsStr,
    ) -> io::Result<(File, super::GenerationResidence)> {
        Err(unsupported())
    }

    pub(super) fn open_file_at(_parent: &File, _name: &OsStr) -> io::Result<File> {
        Err(unsupported())
    }

    pub(super) fn open_file_at_readonly(_parent: &File, _name: &OsStr) -> io::Result<File> {
        Err(unsupported())
    }

    pub(super) fn open_file_at_delete(_parent: &File, _name: &OsStr) -> io::Result<File> {
        Err(unsupported())
    }

    pub(super) fn remove_file_at(_exact: &File, _parent: &File, _name: &OsStr) -> io::Result<()> {
        Err(unsupported())
    }

    pub(super) fn publish_file_at(
        _source: &File,
        _destination: &File,
        _name: &OsStr,
    ) -> io::Result<()> {
        Err(unsupported())
    }

    pub(super) fn promote_file_at(
        _source: &File,
        _source_parent: &File,
        _source_name: &OsStr,
        _source_is_named: bool,
        _destination: &File,
        _destination_name: &OsStr,
    ) -> io::Result<()> {
        Err(unsupported())
    }

    pub(super) fn sync_directory(_directory: &File) -> io::Result<()> {
        Err(unsupported())
    }

    pub(super) fn validate_native_basename(_name: &OsStr) -> io::Result<()> {
        Err(unsupported())
    }
}

#[cfg(test)]
#[path = "pinned/tests.rs"]
mod tests;
