//! Bounded append-only save journal with self-identified file generations.

use super::*;

pub(super) fn create_journal(path: &Path, intent: &mut SaveIntent) -> io::Result<()> {
    let parent_path = path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "recording journal has no parent",
        )
    })?;
    let parent = pin_private_absolute_dir(parent_path, intent.journal_parent_identity)?;
    let basename = path.file_name().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "recording journal has no basename",
        )
    })?;
    let mut generation = parent.create_new(basename)?;
    intent.journal_identity = Some(generation.identity());
    let mut bytes = serde_json::to_vec(intent).map_err(io::Error::other)?;
    bytes.push(b'\n');
    generation.file_mut()?.write_all(&bytes)?;
    generation.sync_durable()
}

pub(super) fn remove_uninitialized_journal(path: &Path, intent: &SaveIntent) -> io::Result<()> {
    let parent_path = path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "recording journal has no parent",
        )
    })?;
    let basename = path.file_name().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "recording journal has no basename",
        )
    })?;
    let parent = pin_private_absolute_dir(parent_path, intent.journal_parent_identity)?;
    let journal = parent.open_existing_child(
        basename,
        intent.journal_identity.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::Unsupported,
                "failed initial journal has no exact file identity",
            )
        })?,
    )?;
    let mut bytes = Vec::new();
    journal
        .file()?
        .take((MAX_JOURNAL_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_JOURNAL_BYTES || parse_journal_records(&bytes).is_ok() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "failed initial journal contains a parseable intent and remains recovery-owned",
        ));
    }
    journal.remove_from_private_parent()
}

pub(super) fn write_journal(path: &Path, intent: &SaveIntent) -> io::Result<()> {
    let parent_identity = intent.journal_parent_identity.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::Unsupported,
            "legacy recording journal has no parent identity",
        )
    })?;
    let journal_identity = intent.journal_identity.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::Unsupported,
            "legacy recording journal has no file identity",
        )
    })?;
    let parent_path = path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "recording journal has no parent",
        )
    })?;
    let basename = path.file_name().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "recording journal has no basename",
        )
    })?;
    let parent = pin_private_absolute_dir(parent_path, Some(parent_identity))?;
    let mut generation = parent.open_existing_child(basename, journal_identity)?;
    let mut current = Vec::new();
    {
        let mut file = generation.file()?;
        file.seek(SeekFrom::Start(0))?;
        file.read_to_end(&mut current)?;
    }
    if parse_journal_records(&current)? == *intent {
        return Ok(());
    }
    if journal_record_count(&current) >= MAX_JOURNAL_RECORDS {
        return Err(io::Error::new(
            io::ErrorKind::StorageFull,
            "pending recording journal exhausted its finite state-transition budget",
        ));
    }
    let mut bytes = serde_json::to_vec(intent).map_err(io::Error::other)?;
    bytes.insert(0, b'\n');
    bytes.push(b'\n');
    let projected = current
        .len()
        .checked_add(bytes.len())
        .ok_or_else(|| io::Error::other("recording journal size overflow"))?;
    if projected > MAX_JOURNAL_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::StorageFull,
            "pending recording journal append log is full",
        ));
    }
    let file = generation.file_mut()?;
    file.seek(SeekFrom::End(0))?;
    file.write_all(&bytes)?;
    generation.sync_durable()
}

pub(super) fn load_journal(path: &Path) -> io::Result<SaveIntent> {
    let mut initial = safe_fs::open_regular_no_symlink(path)?;
    let observed_identity = safe_fs::file_object_id(&initial)?;
    let mut bytes = Vec::new();
    initial.read_to_end(&mut bytes)?;
    if bytes.len() > MAX_JOURNAL_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "pending recording journal is too large",
        ));
    }
    let mut intent = parse_journal_records(&bytes)?;
    if let (Some(parent_identity), Some(journal_identity)) =
        (intent.journal_parent_identity, intent.journal_identity)
    {
        if observed_identity != journal_identity {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "recording journal pathname names a foreign file generation",
            ));
        }
        let parent_path = path.parent().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "recording journal has no parent",
            )
        })?;
        let basename = path.file_name().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "recording journal has no basename",
            )
        })?;
        let parent = pin_private_absolute_dir(parent_path, Some(parent_identity))?;
        let exact = parent.open_existing_child(basename, journal_identity)?;
        let mut exact_file = exact.file()?;
        let mut exact_bytes = Vec::new();
        exact_file.read_to_end(&mut exact_bytes)?;
        if exact_bytes.len() > MAX_JOURNAL_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "pending recording journal is too large",
            ));
        }
        intent = parse_journal_records(&exact_bytes)?;
        if intent.journal_identity != Some(exact.identity()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "recording journal self identity does not match its open handle",
            ));
        }
    }
    if intent.schema != JOURNAL_SCHEMA {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unsupported pending recording schema {}", intent.schema),
        ));
    }
    validate_intent(&intent)?;
    Ok(intent)
}

fn parse_journal_records(bytes: &[u8]) -> io::Result<SaveIntent> {
    if let Ok(intent) = serde_json::from_slice(bytes) {
        return Ok(intent);
    }
    let mut latest = None;
    for line in bytes.split_inclusive(|byte| *byte == b'\n') {
        if !line.ends_with(b"\n") {
            continue;
        }
        let line = line[..line.len() - 1].trim_ascii();
        if !line.is_empty()
            && let Ok(intent) = serde_json::from_slice::<SaveIntent>(line)
        {
            latest = Some(intent);
        }
    }
    latest.ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid recording journal"))
}

fn journal_record_count(bytes: &[u8]) -> usize {
    if serde_json::from_slice::<SaveIntent>(bytes).is_ok() {
        return 1;
    }
    bytes
        .split_inclusive(|byte| *byte == b'\n')
        .filter(|line| {
            line.ends_with(b"\n")
                && serde_json::from_slice::<SaveIntent>(line[..line.len() - 1].trim_ascii()).is_ok()
        })
        .count()
}

pub(super) fn validate_intent(intent: &SaveIntent) -> io::Result<()> {
    let token_valid =
        intent.token.len() == 32 && intent.token.bytes().all(|byte| byte.is_ascii_hexdigit());
    let filename_valid = !intent.filename.is_empty()
        && intent.filename.len() <= 200
        && intent.filename != "."
        && intent.filename != ".."
        && !intent
            .filename
            .chars()
            .any(|character| matches!(character, '/' | '\\'));
    let ext_valid = !intent.ext.is_empty()
        && intent.ext.len() <= 10
        && intent.ext.bytes().all(|byte| byte.is_ascii_alphanumeric());
    let identity_valid = intent.commit_identity.as_ref().is_none_or(|identity| {
        identity.sha256.len() == 64 && identity.sha256.bytes().all(|byte| byte.is_ascii_hexdigit())
    });
    let commit_fields_coupled = intent.commit_identity.is_none()
        || (intent.destination.is_some() && intent.stage_identity.is_some());
    let destination_identity_coupled =
        intent.destination_identity.is_none() || intent.commit_identity.is_some();
    if token_valid
        && filename_valid
        && ext_valid
        && identity_valid
        && commit_fields_coupled
        && destination_identity_coupled
    {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid pending recording journal fields",
        ))
    }
}

pub(super) fn validate_journal_budget(intent: &SaveIntent) -> io::Result<()> {
    let initial = serde_json::to_vec(intent).map_err(io::Error::other)?.len();
    let worst_snapshot = initial
        .checked_add(MAX_SNAPSHOT_GROWTH_BYTES)
        .ok_or_else(|| io::Error::other("recording journal budget overflow"))?;
    // Appends carry a leading delimiter (which terminates a torn tail) and a trailing newline.
    // Charging both bytes to every record safely overcounts the initial record by one byte.
    let framed_snapshot = worst_snapshot
        .checked_add(2)
        .ok_or_else(|| io::Error::other("recording journal budget overflow"))?;
    let worst_log = framed_snapshot
        .checked_mul(MAX_JOURNAL_RECORDS)
        .ok_or_else(|| io::Error::other("recording journal budget overflow"))?;
    if worst_log <= MAX_JOURNAL_BYTES {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "recording paths and metadata exceed the bounded recovery journal budget",
        ))
    }
}

pub(super) fn same_request(left: &SaveIntent, right: &SaveIntent) -> bool {
    left.schema == right.schema
        && left.token == right.token
        && left.id == right.id
        && left.source == right.source
        && left.final_dir == right.final_dir
        && left.filename == right.filename
        && left.ext == right.ext
        && left.title == right.title
        && left.artist == right.artist
        && left.station == right.station
        && left.source_parent_identity == right.source_parent_identity
        && left.source_identity == right.source_identity
        && left.final_parent_identity == right.final_parent_identity
        && left.work_parent_identity == right.work_parent_identity
        && left.save_lock_identity == right.save_lock_identity
        && left.journal_parent_identity == right.journal_parent_identity
        && left.journal_identity == right.journal_identity
}

pub(super) fn ensure_identity_contract(intent: &SaveIntent) -> io::Result<()> {
    let complete = intent.source_parent_identity.is_some()
        && intent.source_identity.is_some()
        && intent.final_parent_identity.is_some()
        && intent.work_parent_identity.is_some()
        && intent.save_lock_identity.is_some()
        && intent.journal_parent_identity.is_some()
        && intent.journal_identity.is_some();
    if complete {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "legacy recording journal lacks kernel identities; bytes were preserved for manual recovery",
        ))
    }
}
