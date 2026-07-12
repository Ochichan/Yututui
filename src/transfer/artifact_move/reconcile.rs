use std::io::{Seek as _, SeekFrom};

use super::*;

pub(super) struct ReconcileFilePolicy<'a> {
    pub(super) fault: Option<&'a dyn Fn(ArtifactMoveFaultPoint) -> std::io::Result<()>>,
    pub(super) before_publish: ArtifactMoveFaultPoint,
    pub(super) after_publish: ArtifactMoveFaultPoint,
    pub(super) retained_source_boundary: ArtifactMoveFaultPoint,
    pub(super) max_bytes: u64,
}

pub(super) struct PinnedTransactionScopes {
    pub(super) source_parent: safe_fs::PinnedDir,
    pub(super) destination_parent: safe_fs::PinnedDir,
}

pub(super) struct ReconcileFilePair<'a> {
    pub(super) source_parent: &'a safe_fs::PinnedDir,
    pub(super) source_name: &'a std::ffi::OsStr,
    pub(super) expected_source_object: safe_fs::FileObjectId,
    pub(super) destination_parent: &'a safe_fs::PinnedDir,
    pub(super) destination_stage_name: &'a std::ffi::OsStr,
    pub(super) expected_stage_object: Option<safe_fs::FileObjectId>,
    pub(super) destination_name: &'a std::ffi::OsStr,
    pub(super) expected: &'a ArtifactIdentity,
}

pub(super) fn reconcile_file_pair(
    pair: ReconcileFilePair<'_>,
    record_stage_object: &mut dyn FnMut(safe_fs::FileObjectId) -> std::io::Result<()>,
    policy: ReconcileFilePolicy<'_>,
) -> std::io::Result<()> {
    let ReconcileFilePair {
        source_parent,
        source_name,
        expected_source_object,
        destination_parent,
        destination_stage_name,
        expected_stage_object,
        destination_name,
        expected,
    } = pair;
    let source =
        match source_parent.open_existing_child_readonly(source_name, expected_source_object) {
            Ok(generation) => Some(bound_artifact(
                generation,
                Path::new(source_name),
                policy.max_bytes,
            )?),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error),
        };
    let destination = match destination_parent.open_child_readonly(destination_name) {
        Ok(generation) => Some(bound_artifact(
            generation,
            Path::new(destination_name),
            policy.max_bytes,
        )?),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error),
    };

    if let Some(destination) = destination.as_ref() {
        if destination.identity != *expected {
            return Err(identity_conflict(
                "destination",
                Path::new(destination_name),
                expected,
                &destination.identity,
            ));
        }
        if let Some(source) = source.as_ref()
            && source.identity != *expected
        {
            return Err(identity_conflict(
                "source",
                Path::new(source_name),
                expected,
                &source.identity,
            ));
        }
        if source
            .as_ref()
            .is_some_and(|source| source.generation.identity() == destination.generation.identity())
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "final destination aliases the retained source object",
            ));
        }
        if let Some(stage_object) = expected_stage_object {
            match destination_parent
                .open_existing_child_readonly(destination_stage_name, stage_object)
            {
                Ok(_) => {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::AlreadyExists,
                        "exact destination exists while a journal-owned named stage is retained",
                    ));
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
        }
        return Ok(());
    }

    let stage = match expected_stage_object {
        Some(expected_object) => {
            #[cfg(windows)]
            let reopened = destination_parent
                .open_existing_recoverable_child(destination_stage_name, expected_object);
            #[cfg(not(windows))]
            let reopened =
                destination_parent.open_existing_child(destination_stage_name, expected_object);
            match reopened {
                Ok(generation) => Some(generation),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                Err(error) => return Err(error),
            }
        }
        None => None,
    };
    let stage = match (stage, source.as_ref()) {
        (Some(generation), source) => {
            let mut stage = bound_artifact(
                generation,
                Path::new(destination_stage_name),
                policy.max_bytes,
            )?;
            if source
                .is_some_and(|source| source.generation.identity() == stage.generation.identity())
            {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::AlreadyExists,
                    "destination stage aliases the retained source object",
                ));
            }
            if stage.identity != *expected {
                let Some(source) = source else {
                    return Err(identity_conflict(
                        "destination stage",
                        Path::new(destination_stage_name),
                        expected,
                        &stage.identity,
                    ));
                };
                stage = copy_destination_stage(
                    source,
                    stage.generation,
                    destination_stage_name,
                    expected,
                    policy.max_bytes,
                )?;
            }
            stage
        }
        (None, Some(_)) => {
            // macOS can clone directly from the exact source handle into the final name. This is
            // independent copy-on-write publication and leaves no destination stage. Filesystems
            // without clone support fall through to the recoverable copy stage below.
            #[cfg(target_os = "macos")]
            inject(policy.fault, policy.before_publish)?;
            if let Some(published) = try_publish_independent_clone(
                source.as_ref().expect("source matched above"),
                destination_parent,
                destination_name,
            )? {
                return validate_published_pair(
                    source.as_ref(),
                    destination_parent,
                    destination_name,
                    expected,
                    policy,
                    published,
                );
            }
            create_destination_stage(
                source.as_ref().expect("source matched above"),
                destination_parent,
                destination_stage_name,
                expected,
                policy.max_bytes,
                record_stage_object,
            )?
        }
        (None, None) => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!(
                    "artifact is missing from source, destination stage, and final name: {}",
                    Path::new(destination_name).display()
                ),
            ));
        }
    };

    inject(policy.fault, policy.before_publish)?;
    let stage_retained_on_error = stage.generation.retains_name_on_drop();
    let published = match stage
        .generation
        .promote_noreplace(destination_parent, destination_name)
    {
        Ok(published) => published,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            let collision = destination_parent.open_child_readonly(destination_name)?;
            let collision =
                bound_artifact(collision, Path::new(destination_name), policy.max_bytes)?;
            if collision.identity == *expected && !stage_retained_on_error {
                return Ok(());
            }
            if collision.identity == *expected {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::AlreadyExists,
                    "exact destination collision retained a journal-owned named stage for explicit recovery",
                ));
            }
            return Err(error);
        }
        Err(error) => return Err(error),
    };
    validate_published_pair(
        source.as_ref(),
        destination_parent,
        destination_name,
        expected,
        policy,
        published,
    )
}

fn validate_published_pair(
    source: Option<&BoundArtifact>,
    destination_parent: &safe_fs::PinnedDir,
    destination_name: &std::ffi::OsStr,
    expected: &ArtifactIdentity,
    policy: ReconcileFilePolicy<'_>,
    published: safe_fs::OwnedGeneration,
) -> std::io::Result<()> {
    if source.is_some_and(|source| source.generation.identity() == published.identity()) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "published destination aliases the retained source object",
        ));
    }
    let published_object = published.identity();
    let published = bound_artifact(published, Path::new(destination_name), policy.max_bytes)?;
    if published.identity != *expected {
        return Err(identity_conflict(
            "published destination",
            Path::new(destination_name),
            expected,
            &published.identity,
        ));
    }

    inject(policy.fault, policy.after_publish)?;
    let reopened =
        destination_parent.open_existing_child_readonly(destination_name, published_object)?;
    let reopened = bound_artifact(reopened, Path::new(destination_name), policy.max_bytes)?;
    if reopened.identity != *expected {
        return Err(identity_conflict(
            "reopened destination",
            Path::new(destination_name),
            expected,
            &reopened.identity,
        ));
    }

    inject(policy.fault, policy.retained_source_boundary)?;
    destination_parent
        .open_existing_child_readonly(destination_name, published_object)
        .map(|_| ())
}

#[cfg(target_os = "macos")]
fn try_publish_independent_clone(
    source: &BoundArtifact,
    destination_parent: &safe_fs::PinnedDir,
    destination_name: &std::ffi::OsStr,
) -> std::io::Result<Option<safe_fs::OwnedGeneration>> {
    match source
        .generation
        .publish_noreplace(destination_parent, destination_name)
    {
        Ok(published) => Ok(Some(published)),
        Err(error)
            if matches!(
                error.raw_os_error(),
                Some(libc::EOPNOTSUPP | libc::EXDEV | libc::EINVAL)
            ) =>
        {
            Ok(None)
        }
        Err(error) => Err(error),
    }
}

#[cfg(not(target_os = "macos"))]
fn try_publish_independent_clone(
    _source: &BoundArtifact,
    _destination_parent: &safe_fs::PinnedDir,
    _destination_name: &std::ffi::OsStr,
) -> std::io::Result<Option<safe_fs::OwnedGeneration>> {
    Ok(None)
}

pub(super) struct ReconcileOptionalFilePair<'a> {
    pub(super) source_parent: &'a safe_fs::PinnedDir,
    pub(super) source_name: &'a std::ffi::OsStr,
    pub(super) expected_source_object: Option<safe_fs::FileObjectId>,
    pub(super) destination_parent: &'a safe_fs::PinnedDir,
    pub(super) destination_stage_name: &'a std::ffi::OsStr,
    pub(super) expected_stage_object: Option<safe_fs::FileObjectId>,
    pub(super) destination_name: &'a std::ffi::OsStr,
    pub(super) expected: Option<&'a ArtifactIdentity>,
}

pub(super) fn reconcile_optional_file_pair(
    pair: ReconcileOptionalFilePair<'_>,
    record_stage_object: &mut dyn FnMut(safe_fs::FileObjectId) -> std::io::Result<()>,
    fault: Option<&dyn Fn(ArtifactMoveFaultPoint) -> std::io::Result<()>>,
) -> std::io::Result<()> {
    let ReconcileOptionalFilePair {
        source_parent,
        source_name,
        expected_source_object,
        destination_parent,
        destination_stage_name,
        expected_stage_object,
        destination_name,
        expected,
    } = pair;
    match (expected_source_object, expected) {
        (Some(object), Some(expected)) => reconcile_file_pair(
            ReconcileFilePair {
                source_parent,
                source_name,
                expected_source_object: object,
                destination_parent,
                destination_stage_name,
                expected_stage_object,
                destination_name,
                expected,
            },
            record_stage_object,
            ReconcileFilePolicy {
                fault,
                before_publish: ArtifactMoveFaultPoint::BeforeSidecarPublish,
                after_publish: ArtifactMoveFaultPoint::AfterSidecarPublishBeforeSourceUnlink,
                retained_source_boundary:
                    ArtifactMoveFaultPoint::BeforeSidecarSourceUnlinkValidation,
                max_bytes: ARTIFACT_SIDECAR_MAX_BYTES,
            },
        ),
        (None, None) => {
            let source = match source_parent.open_child_readonly(source_name) {
                Ok(_) => true,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
                Err(error) => return Err(error),
            };
            let destination = match destination_parent.open_child_readonly(destination_name) {
                Ok(_) => true,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
                Err(error) => return Err(error),
            };
            let stage = match destination_parent.open_child(destination_stage_name) {
                Ok(_) => true,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
                Err(error) => return Err(error),
            };
            if source || destination || stage {
                Err(std::io::Error::new(
                    std::io::ErrorKind::AlreadyExists,
                    "unexpected sidecar appeared in a pinned artifact directory",
                ))
            } else {
                Ok(())
            }
        }
        _ => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "sidecar content and object identities are inconsistent",
        )),
    }
}

struct BoundArtifact {
    generation: safe_fs::OwnedGeneration,
    identity: ArtifactIdentity,
}

fn create_destination_stage(
    source: &BoundArtifact,
    destination_parent: &safe_fs::PinnedDir,
    stage_name: &std::ffi::OsStr,
    expected: &ArtifactIdentity,
    max_bytes: u64,
    record_stage_object: &mut dyn FnMut(safe_fs::FileObjectId) -> std::io::Result<()>,
) -> std::io::Result<BoundArtifact> {
    if source.identity != *expected {
        return Err(identity_conflict(
            "source",
            Path::new(source.generation.basename()),
            expected,
            &source.identity,
        ));
    }
    let stage = destination_parent.create_ephemeral(stage_name)?;
    if stage.identity() == source.generation.identity() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "new destination stage unexpectedly aliases its source",
        ));
    }
    record_stage_object(stage.identity())?;
    copy_destination_stage(source, stage, stage_name, expected, max_bytes)
}

fn copy_destination_stage(
    source: &BoundArtifact,
    mut stage: safe_fs::OwnedGeneration,
    stage_name: &std::ffi::OsStr,
    expected: &ArtifactIdentity,
    max_bytes: u64,
) -> std::io::Result<BoundArtifact> {
    if source.identity != *expected {
        return Err(identity_conflict(
            "source",
            Path::new(source.generation.basename()),
            expected,
            &source.identity,
        ));
    }
    {
        let destination = stage.file_mut()?;
        destination.set_len(0)?;
        destination.seek(SeekFrom::Start(0))?;
    }
    let mut source_file = source.generation.file()?.try_clone()?;
    source_file.seek(SeekFrom::Start(0))?;
    let copied = {
        let limit = expected
            .len
            .checked_add(1)
            .ok_or_else(|| std::io::Error::other("artifact copy size overflow"))?;
        std::io::copy(&mut source_file.take(limit), stage.file_mut()?)?
    };
    if copied != expected.len {
        return Err(std::io::Error::other(
            "artifact source changed while copying to destination stage",
        ));
    }
    stage.sync_durable()?;
    let stage = bound_artifact(stage, Path::new(stage_name), max_bytes)?;
    if stage.identity != *expected {
        return Err(identity_conflict(
            "destination stage",
            Path::new(stage_name),
            expected,
            &stage.identity,
        ));
    }
    Ok(stage)
}

fn bound_artifact(
    generation: safe_fs::OwnedGeneration,
    label: &Path,
    max_bytes: u64,
) -> std::io::Result<BoundArtifact> {
    let mut file = generation.file()?.try_clone()?;
    let identity = file_identity_from_open(&mut file, label, max_bytes)?;
    Ok(BoundArtifact {
        generation,
        identity,
    })
}

pub(super) fn pin_transaction_scopes(
    txn: &ArtifactMoveTxn,
) -> anyhow::Result<PinnedTransactionScopes> {
    let source_parent = match (txn.source_private_root.as_ref(), txn.source_private_root_id) {
        (Some(private_root), Some(expected_root)) => {
            let root = safe_fs::PinnedDir::open_private_existing(private_root, Path::new(""))
                .with_context(|| format!("pin private source root {}", private_root.display()))?;
            if root.identity() != expected_root {
                bail!(
                    "artifact private source root object changed: {}",
                    private_root.display()
                );
            }
            let relative = txn
                .source_parent
                .strip_prefix(private_root)
                .context("private artifact source parent escaped its root")?;
            safe_fs::PinnedDir::open_private_existing(private_root, relative).with_context(
                || format!("pin private source parent {}", txn.source_parent.display()),
            )?
        }
        (None, None) => safe_fs::PinnedDir::open_existing(&txn.source_parent, Path::new(""))
            .with_context(|| format!("pin source parent {}", txn.source_parent.display()))?,
        _ => bail!("artifact private source root path/object identity is inconsistent"),
    };
    if source_parent.identity() != txn.source_parent_id {
        bail!(
            "artifact source parent object changed: {}",
            txn.source_parent.display()
        );
    }
    let destination_root = match (
        txn.destination_private_root.as_ref(),
        txn.destination_private_root_id,
    ) {
        (Some(private_root), Some(expected_root)) => {
            let root = safe_fs::PinnedDir::open_private_existing(private_root, Path::new(""))
                .with_context(|| {
                    format!("pin private destination root {}", private_root.display())
                })?;
            if root.identity() != expected_root {
                bail!(
                    "artifact private destination root object changed: {}",
                    private_root.display()
                );
            }
            let relative = txn
                .destination_root
                .strip_prefix(private_root)
                .context("private artifact destination root escaped its scope")?;
            safe_fs::PinnedDir::open_private_existing(private_root, relative).with_context(
                || {
                    format!(
                        "pin private destination scope {}",
                        txn.destination_root.display()
                    )
                },
            )?
        }
        (None, None) => safe_fs::PinnedDir::open_existing(&txn.destination_root, Path::new(""))
            .with_context(|| format!("pin destination root {}", txn.destination_root.display()))?,
        _ => bail!("artifact private destination root path/object identity is inconsistent"),
    };
    if destination_root.identity() != txn.destination_root_id {
        bail!(
            "artifact destination root object changed: {}",
            txn.destination_root.display()
        );
    }
    let destination_parent = match txn.destination_private_root.as_ref() {
        Some(private_root) => {
            let relative = txn
                .destination_parent
                .strip_prefix(private_root)
                .context("private artifact destination parent escaped its scope")?;
            safe_fs::PinnedDir::open_private_existing(private_root, relative).with_context(
                || {
                    format!(
                        "pin private destination parent {}",
                        txn.destination_parent.display()
                    )
                },
            )?
        }
        None => {
            let relative = txn
                .destination_parent
                .strip_prefix(&txn.destination_root)
                .context("artifact destination parent escaped its root")?;
            safe_fs::PinnedDir::open_existing(&txn.destination_root, relative).with_context(
                || {
                    format!(
                        "pin destination parent {}",
                        txn.destination_parent.display()
                    )
                },
            )?
        }
    };
    if destination_parent.identity() != txn.destination_parent_id {
        bail!(
            "artifact destination parent object changed: {}",
            txn.destination_parent.display()
        );
    }
    Ok(PinnedTransactionScopes {
        source_parent,
        destination_parent,
    })
}
