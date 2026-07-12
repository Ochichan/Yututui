//! Handle-pinned source, stage, destination, and private-generation cleanup.

use super::*;
use sha2::{Digest, Sha256};

struct CleanupProof {
    source: Option<safe_fs::OwnedGeneration>,
    stage: Option<safe_fs::OwnedGeneration>,
    journal: safe_fs::OwnedGeneration,
}

pub(super) fn reclaim_settled_generations(save: &AcceptedSave) -> io::Result<()> {
    let proof = cleanup_proof(save)?;
    if let Some(source) = proof.source {
        source.remove_from_private_parent()?;
    }
    if let Some(stage) = proof.stage {
        stage.remove_from_private_parent()?;
    }
    proof.journal.remove_from_private_parent()
}

fn cleanup_proof(save: &AcceptedSave) -> io::Result<CleanupProof> {
    let intent = &save.intent;
    let source_parent_path = intent.source.parent().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidData, "recording source has no parent")
    })?;
    let source_parent =
        pin_private_absolute_dir(source_parent_path, intent.source_parent_identity)?;
    let source = open_optional_exact_child(
        &source_parent,
        intent.source.file_name().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "recording source has no basename",
            )
        })?,
        intent.source_identity.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::Unsupported,
                "recording source identity is missing",
            )
        })?,
    )?;

    let work_parent =
        pin_private_absolute_dir(&work_dir(&intent.final_dir), intent.work_parent_identity)?;
    let stage_path = stage_path(intent);
    let stage = open_optional_exact_child(
        &work_parent,
        stage_path.file_name().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "recording stage has no basename",
            )
        })?,
        intent.stage_identity.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::Unsupported,
                "recording stage identity is missing",
            )
        })?,
    )?;

    let journal_parent_path = save.journal_path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "recording journal has no parent",
        )
    })?;
    let journal_parent =
        pin_private_absolute_dir(journal_parent_path, intent.journal_parent_identity)?;
    let journal = journal_parent.open_existing_child(
        save.journal_path.file_name().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "recording journal has no basename",
            )
        })?,
        intent.journal_identity.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::Unsupported,
                "recording journal identity is missing",
            )
        })?,
    )?;
    Ok(CleanupProof {
        source,
        stage,
        journal,
    })
}

fn open_optional_exact_child(
    parent: &safe_fs::PinnedDir,
    basename: &std::ffi::OsStr,
    identity: safe_fs::FileObjectId,
) -> io::Result<Option<safe_fs::OwnedGeneration>> {
    match parent.open_existing_child(basename, identity) {
        Ok(generation) => Ok(Some(generation)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

pub(super) fn prepare_or_reuse_stage(
    save: &mut AcceptedSave,
    work_parent: &safe_fs::PinnedDir,
) -> io::Result<PendingStage> {
    let path = stage_path(&save.intent);
    let basename = path.file_name().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "recording stage has no basename",
        )
    })?;
    let generation = match save.intent.stage_identity {
        Some(expected) => match work_parent.open_existing_child(basename, expected) {
            Ok(generation) => generation,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                // A no-replace promotion can cross the rename boundary and then fail a
                // directory sync. If neither name survives a crash, the exact source is still
                // recovery-owned, so create and journal a fresh private stage generation.
                let generation = work_parent.create_new(basename)?;
                save.intent.stage_identity = Some(generation.identity());
                write_journal(&save.journal_path, &save.intent)?;
                generation
            }
            Err(error) => return Err(error),
        },
        None => {
            let generation = work_parent.create_new(basename)?;
            save.intent.stage_identity = Some(generation.identity());
            write_journal(&save.journal_path, &save.intent)?;
            generation
        }
    };
    let mut stage = PendingStage::new(path, generation);
    if let Some(expected) = save.intent.commit_identity.as_ref()
        && file_identity_handle(stage.generation.file()?)? == *expected
    {
        return Ok(stage);
    }
    rebuild_stage(&save.intent, &mut stage)?;
    Ok(stage)
}

fn rebuild_stage(intent: &SaveIntent, stage: &mut PendingStage) -> io::Result<()> {
    let source_parent_path = intent.source.parent().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidData, "recording source has no parent")
    })?;
    let source_name = intent.source.file_name().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "recording source has no basename",
        )
    })?;
    let source_parent =
        pin_private_absolute_dir(source_parent_path, intent.source_parent_identity)?;
    let source = source_parent.open_existing_child_readonly(
        source_name,
        intent.source_identity.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::Unsupported,
                "recording source identity is missing",
            )
        })?,
    )?;
    let source_permissions = source.file()?.metadata()?.permissions();
    let mut source_file = source.file()?;
    source_file.seek(SeekFrom::Start(0))?;
    {
        let output = stage.generation.file_mut()?;
        output.set_len(0)?;
        output.seek(SeekFrom::Start(0))?;
        io::copy(&mut source_file, output)?;
        output.sync_all()?;
        if ext_is_taggable(&intent.ext)
            && super::super::tag_file_handle(
                output,
                intent.title.as_deref(),
                intent.artist.as_deref(),
                intent.station.as_deref(),
            )
            .is_err()
        {
            source_file.seek(SeekFrom::Start(0))?;
            output.set_len(0)?;
            output.seek(SeekFrom::Start(0))?;
            io::copy(&mut source_file, output)?;
        }
        output.set_permissions(source_permissions)?;
    }
    stage.generation.sync_durable()
}

#[cfg(test)]
pub(super) fn prepare_stage(intent: &SaveIntent) -> io::Result<PendingStage> {
    let work_parent =
        pin_private_absolute_dir(&work_dir(&intent.final_dir), intent.work_parent_identity)?;
    let path = stage_path(intent);
    let basename = path.file_name().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "recording stage has no basename",
        )
    })?;
    let generation = work_parent.create_new(basename)?;
    let mut stage = PendingStage::new(path, generation);
    rebuild_stage(intent, &mut stage)?;
    Ok(stage)
}

pub(super) fn pinned_name_occupied(parent: &safe_fs::PinnedDir, path: &Path) -> io::Result<bool> {
    let basename = path.file_name().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "recording destination has no basename",
        )
    })?;
    match parent.open_child(basename) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) if error.kind() == io::ErrorKind::InvalidData => Ok(true),
        Err(error) => Err(error),
    }
}

pub(super) fn destination_state_pinned(
    intent: &SaveIntent,
    parent: &safe_fs::PinnedDir,
    path: &Path,
    expected: &CommitIdentity,
) -> io::Result<DestinationState> {
    if path.parent() != Some(intent.final_dir.as_path()) {
        return Ok(DestinationState::Foreign);
    }
    let basename = path.file_name().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "recording destination has no basename",
        )
    })?;
    let generation = match intent.destination_identity {
        Some(identity) => parent.open_existing_child(basename, identity),
        None => parent.open_child(basename),
    };
    let generation = match generation {
        Ok(generation) => generation,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(DestinationState::Missing);
        }
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::AlreadyExists | io::ErrorKind::InvalidData
            ) =>
        {
            return Ok(DestinationState::Foreign);
        }
        Err(error) => return Err(error),
    };
    if intent.destination_identity.is_none() && intent.stage_identity != Some(generation.identity())
    {
        return Ok(DestinationState::Foreign);
    }
    if file_identity_handle(generation.file()?)? == *expected {
        Ok(DestinationState::Matches)
    } else {
        Ok(DestinationState::Foreign)
    }
}

pub(super) fn file_identity_handle(file: &std::fs::File) -> io::Result<CommitIdentity> {
    let mut file = file;
    file.seek(SeekFrom::Start(0))?;
    let mut len = 0u64;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        len = len
            .checked_add(read as u64)
            .ok_or_else(|| io::Error::other("recording size overflow"))?;
        hasher.update(&buffer[..read]);
    }
    let digest = hasher.finalize();
    file.seek(SeekFrom::Start(0))?;
    Ok(CommitIdentity {
        len,
        sha256: digest.iter().map(|byte| format!("{byte:02x}")).collect(),
    })
}

pub(super) fn pin_absolute_dir(
    path: &Path,
    expected: Option<safe_fs::FileObjectId>,
) -> io::Result<safe_fs::PinnedDir> {
    let pinned = safe_fs::PinnedDir::open_existing(path, Path::new(""))?;
    if expected.is_some_and(|expected| pinned.identity() != expected) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("directory generation changed at {}", path.display()),
        ));
    }
    Ok(pinned)
}

pub(super) fn pin_private_absolute_dir(
    path: &Path,
    expected: Option<safe_fs::FileObjectId>,
) -> io::Result<safe_fs::PinnedDir> {
    let pinned = safe_fs::PinnedDir::open_private_existing(path, Path::new(""))?;
    if expected.is_some_and(|expected| pinned.identity() != expected) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("private directory generation changed at {}", path.display()),
        ));
    }
    Ok(pinned)
}
