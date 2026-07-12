//! Preview path planning for import-session files before committing them to a library root.

use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context as _, bail};

use super::checkpoint::ReviewDecision;
use super::session::{ImportSession, ImportSessionRow, ImportSessionRowStatus};

pub const DEFAULT_IMPORT_ORGANIZE_TEMPLATE: &str =
    crate::config::LOCAL_IMPORT_PATH_TEMPLATE_DEFAULT;

#[derive(Debug, Clone)]
pub struct ImportOrganizeOptions {
    pub root: PathBuf,
    pub template: String,
}

impl ImportOrganizeOptions {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            template: DEFAULT_IMPORT_ORGANIZE_TEMPLATE.to_owned(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportOrganizePlan {
    pub session_id: String,
    pub root: PathBuf,
    pub template: String,
    pub rows: Vec<ImportOrganizePlanRow>,
    pub move_count: u32,
    pub already_count: u32,
    pub skipped_count: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportOrganizeApplyReport {
    pub moved_count: u32,
    pub already_count: u32,
    pub skipped_count: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportOrganizePlanRow {
    pub row_id: String,
    pub source_order: u32,
    pub title: String,
    pub current_path: Option<PathBuf>,
    pub target_path: Option<PathBuf>,
    pub decision: ImportOrganizeDecision,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImportOrganizeDecision {
    Move,
    AlreadyAtTarget,
    NotAccepted,
    MissingLocalPath,
}

pub fn build_import_organize_plan(
    session: &ImportSession,
    options: &ImportOrganizeOptions,
) -> anyhow::Result<ImportOrganizePlan> {
    if options.root.as_os_str().is_empty() {
        bail!("organize root must not be empty");
    }
    let root = normalize_root(&options.root)?;
    let template = if options.template.trim().is_empty() {
        DEFAULT_IMPORT_ORGANIZE_TEMPLATE
    } else {
        options.template.trim()
    };

    let mut reserved = HashSet::<PathBuf>::new();
    let mut rows = Vec::with_capacity(session.rows.len());
    let mut move_count = 0u32;
    let mut already_count = 0u32;
    let mut skipped_count = 0u32;

    for row in &session.rows {
        let planned = plan_row(session, row, &root, template, &mut reserved)?;
        match planned.decision {
            ImportOrganizeDecision::Move => move_count += 1,
            ImportOrganizeDecision::AlreadyAtTarget => already_count += 1,
            ImportOrganizeDecision::NotAccepted | ImportOrganizeDecision::MissingLocalPath => {
                skipped_count += 1;
            }
        }
        rows.push(planned);
    }

    Ok(ImportOrganizePlan {
        session_id: session.session_id.clone(),
        root,
        template: template.to_owned(),
        rows,
        move_count,
        already_count,
        skipped_count,
    })
}

pub fn apply_import_organize_plan(
    plan: &ImportOrganizePlan,
) -> anyhow::Result<ImportOrganizeApplyReport> {
    let _guard = super::session::ImportRecordGuard::try_acquire(&plan.session_id)?;
    super::artifact_move::reconcile_session_locked(&plan.session_id)
        .context("reconcile pending artifact moves before organizing")?;
    let current_session = ImportSession::load(&plan.session_id).with_context(|| {
        format!(
            "reload import session {} before organizing",
            plan.session_id
        )
    })?;
    let mut moved_count = 0u32;
    let mut already_count = 0u32;
    let mut skipped_count = 0u32;
    for row in &plan.rows {
        match row.decision {
            ImportOrganizeDecision::Move => {
                let from = row
                    .current_path
                    .as_ref()
                    .context("move row missing current path")?;
                let to = row
                    .target_path
                    .as_ref()
                    .context("move row missing target path")?;
                let already_committed = current_session.rows.iter().any(|current| {
                    current.source_order == row.source_order
                        && current.row_id == row.row_id
                        && current.written
                        && current
                            .local_path
                            .as_deref()
                            .is_some_and(|path| paths_equivalent(path, to))
                        && to.is_file()
                });
                if already_committed {
                    already_count += 1;
                    continue;
                }
                let request = super::artifact_move::ArtifactMoveRequest::organize(
                    plan.session_id.clone(),
                    row.row_id.clone(),
                    row.source_order,
                    from.clone(),
                    to.clone(),
                    plan.root.clone(),
                );
                super::artifact_move::commit_locked(request).with_context(|| {
                    format!(
                        "commit import artifact move {} row #{}",
                        plan.session_id, row.source_order
                    )
                })?;
                moved_count += 1;
            }
            ImportOrganizeDecision::AlreadyAtTarget => already_count += 1,
            ImportOrganizeDecision::NotAccepted | ImportOrganizeDecision::MissingLocalPath => {
                skipped_count += 1;
            }
        }
    }
    Ok(ImportOrganizeApplyReport {
        moved_count,
        already_count,
        skipped_count,
    })
}

fn plan_row(
    session: &ImportSession,
    row: &ImportSessionRow,
    root: &Path,
    template: &str,
    reserved: &mut HashSet<PathBuf>,
) -> anyhow::Result<ImportOrganizePlanRow> {
    let mut warnings = Vec::new();
    let current_path = row.local_path.clone();
    if !is_accepted_row(row) {
        return Ok(plan_skip(
            row,
            current_path,
            ImportOrganizeDecision::NotAccepted,
        ));
    }
    let Some(current_path) = current_path else {
        return Ok(plan_skip(
            row,
            None,
            ImportOrganizeDecision::MissingLocalPath,
        ));
    };

    let relative = render_template(session, row, template);
    let extension = current_path
        .extension()
        .and_then(|ext| ext.to_str())
        .filter(|ext| !ext.trim().is_empty())
        .unwrap_or("m4a");
    let target = unique_target(
        root,
        &relative,
        extension,
        Some(&current_path),
        reserved,
        &mut warnings,
    )?;
    let decision = if paths_equivalent(&current_path, &target) {
        ImportOrganizeDecision::AlreadyAtTarget
    } else {
        ImportOrganizeDecision::Move
    };

    Ok(ImportOrganizePlanRow {
        row_id: row.row_id.clone(),
        source_order: row.source_order,
        title: row.title.clone(),
        current_path: Some(current_path),
        target_path: Some(target),
        decision,
        warnings,
    })
}

fn plan_skip(
    row: &ImportSessionRow,
    current_path: Option<PathBuf>,
    decision: ImportOrganizeDecision,
) -> ImportOrganizePlanRow {
    ImportOrganizePlanRow {
        row_id: row.row_id.clone(),
        source_order: row.source_order,
        title: row.title.clone(),
        current_path,
        target_path: None,
        decision,
        warnings: Vec::new(),
    }
}

fn is_accepted_row(row: &ImportSessionRow) -> bool {
    matches!(row.status, ImportSessionRowStatus::Matched)
        && !matches!(
            row.review_decision,
            Some(ReviewDecision::Rejected | ReviewDecision::Skipped)
        )
}

fn normalize_root(root: &Path) -> anyhow::Result<PathBuf> {
    let mut normalized = PathBuf::new();
    for component in root.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => bail!("organize root must not contain `..`"),
            other => normalized.push(other.as_os_str()),
        }
    }
    if normalized.as_os_str().is_empty() {
        bail!("organize root must not be empty");
    }
    Ok(normalized)
}

fn render_template(session: &ImportSession, row: &ImportSessionRow, template: &str) -> PathBuf {
    let mut relative = PathBuf::new();
    for raw_component in template.split('/') {
        let rendered = render_component(session, row, raw_component);
        relative.push(sanitize_component(&rendered));
    }
    relative
}

fn render_component(session: &ImportSession, row: &ImportSessionRow, component: &str) -> String {
    let mut out = component.to_owned();
    for (token, value) in template_values(session, row) {
        out = out.replace(token, &value);
    }
    out
}

fn template_values(session: &ImportSession, row: &ImportSessionRow) -> Vec<(&'static str, String)> {
    let artist = row
        .artists
        .first()
        .cloned()
        .unwrap_or_else(|| "Unknown Artist".to_owned());
    let artists = if row.artists.is_empty() {
        artist.clone()
    } else {
        row.artists.join(", ")
    };
    let album_artist = if row.album_artists.is_empty() {
        artist.clone()
    } else {
        row.album_artists.join(", ")
    };
    let album = row
        .album
        .clone()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "Unknown Album".to_owned());
    let date = row.album_release_date.clone().unwrap_or_default();
    let year = date
        .get(0..4)
        .filter(|value| value.chars().all(|ch| ch.is_ascii_digit()))
        .unwrap_or("0000")
        .to_owned();
    let youtube_id = selected_key(row).unwrap_or_default().to_owned();
    vec![
        ("{title}", non_empty(&row.title, "Untitled")),
        ("{artist}", artist),
        ("{artists}", artists),
        ("{album}", album),
        ("{album_artist}", album_artist),
        ("{year}", year),
        ("{date}", date),
        ("{track}", format_number(row.track_number)),
        ("{disc}", format_number(row.disc_number)),
        (
            "{disc_track}",
            format_disc_track(row.disc_number, row.track_number),
        ),
        ("{youtube_id}", youtube_id),
        (
            "{spotify_id}",
            spotify_id(&row.source_key).unwrap_or_default(),
        ),
        ("{isrc}", row.isrc.clone().unwrap_or_default()),
        ("{session_id}", session.session_id.clone()),
    ]
}

fn selected_key(row: &ImportSessionRow) -> Option<&str> {
    match &row.review_decision {
        Some(ReviewDecision::Accepted { key, .. }) => Some(key.as_str()),
        _ => row.selected_key.as_deref(),
    }
}

fn non_empty(value: &str, fallback: &str) -> String {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        fallback.to_owned()
    } else {
        trimmed.to_owned()
    }
}

fn spotify_id(source_key: &str) -> Option<String> {
    source_key
        .strip_prefix("spotify:track:")
        .filter(|value| !value.trim().is_empty())
        .map(str::to_owned)
}

fn format_number(value: Option<u32>) -> String {
    value
        .filter(|value| *value > 0)
        .map(|value| format!("{value:02}"))
        .unwrap_or_default()
}

fn format_disc_track(disc: Option<u32>, track: Option<u32>) -> String {
    match (
        disc.filter(|value| *value > 1),
        track.filter(|value| *value > 0),
    ) {
        (Some(disc), Some(track)) => format!("{disc:02}-{track:02}"),
        (None, Some(track)) => format!("{track:02}"),
        (Some(disc), None) => format!("{disc:02}"),
        (None, None) => "00".to_owned(),
    }
}

fn sanitize_component(raw: &str) -> String {
    let mut out = String::new();
    for ch in raw.trim().chars() {
        match ch {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => out.push('_'),
            ch if ch.is_control() => out.push('_'),
            ch => out.push(ch),
        }
    }
    while out.contains("  ") {
        out = out.replace("  ", " ");
    }
    while out.contains("..") {
        out = out.replace("..", ".");
    }
    let out = out.trim_matches([' ', '.']).trim();
    if out.is_empty() || out == "." || out == ".." {
        "_".to_owned()
    } else {
        out.to_owned()
    }
}

fn unique_target(
    root: &Path,
    relative: &Path,
    extension: &str,
    current_path: Option<&Path>,
    reserved: &mut HashSet<PathBuf>,
    warnings: &mut Vec<String>,
) -> anyhow::Result<PathBuf> {
    let mut base = root.to_path_buf();
    for component in relative.components() {
        let Component::Normal(component) = component else {
            continue;
        };
        base.push(component);
    }
    base.set_extension(extension);
    if !base.starts_with(root) {
        bail!("planned path escaped organize root");
    }
    let first = base.clone();
    let mut candidate = first.clone();
    let mut suffix = 2u32;
    while reserved.contains(&candidate)
        || (candidate.exists()
            && !current_path.is_some_and(|current| paths_equivalent(current, &candidate)))
    {
        candidate = suffix_path(&first, suffix)
            .with_context(|| format!("could not suffix target path {}", first.display()))?;
        suffix += 1;
    }
    if candidate != first {
        warnings.push(format!(
            "target collision resolved as {}",
            candidate.display()
        ));
    }
    reserved.insert(candidate.clone());
    Ok(candidate)
}

fn suffix_path(path: &Path, suffix: u32) -> Option<PathBuf> {
    let stem = path.file_stem()?.to_str()?;
    let mut next = path.to_path_buf();
    next.set_file_name(format!("{stem} ({suffix})"));
    if let Some(ext) = path.extension() {
        next.set_extension(ext);
    }
    Some(next)
}

fn paths_equivalent(a: &Path, b: &Path) -> bool {
    a == b
        || match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
            (Ok(a), Ok(b)) => a == b,
            _ => false,
        }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(order: u32, title: &str, path: &str) -> ImportSessionRow {
        ImportSessionRow {
            row_id: format!("row-{order:05}"),
            source_order: order,
            status: ImportSessionRowStatus::Matched,
            title: title.to_owned(),
            artists: vec!["Track Artist".to_owned()],
            album_artists: vec!["Album Artist".to_owned()],
            album: Some("Album".to_owned()),
            album_release_date: Some("2026-07-08".to_owned()),
            disc_number: Some(1),
            track_number: Some(order),
            source_key: format!("spotify:track:sp{order}"),
            selected_key: Some(format!("ytid000000{order}")),
            local_path: Some(PathBuf::from(path)),
            ..ImportSessionRow::default()
        }
    }

    fn session(rows: Vec<ImportSessionRow>) -> ImportSession {
        ImportSession {
            session_id: "sp2yt-20260708-plan".to_owned(),
            rows,
            ..ImportSession::default()
        }
    }

    fn temp_root(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "yututui-organize-plan-{name}-{}",
            std::process::id()
        ))
    }

    fn private_import_session_dir(root: &Path, session_id: &str) -> PathBuf {
        let private_root = root.join(".yututui-inbox");
        crate::util::safe_fs::ensure_private_dir(&private_root)
            .expect("create private import inbox root");
        let session_dir = private_root.join(session_id);
        crate::util::safe_fs::ensure_private_dir(&session_dir)
            .expect("create private import session directory");
        session_dir
    }

    #[test]
    fn default_template_groups_by_album_artist_and_album() {
        let root = temp_root("grouping");
        let session = session(vec![row(3, "Song", "/tmp/inbox/Song.m4a")]);
        let plan = build_import_organize_plan(&session, &ImportOrganizeOptions::new(root.clone()))
            .unwrap();

        assert_eq!(plan.move_count, 1);
        assert_eq!(
            plan.rows[0].target_path.as_deref(),
            Some(
                root.join("Album Artist")
                    .join("2026 - Album")
                    .join("03 - Song [ytid0000003].m4a")
                    .as_path()
            )
        );
    }

    #[test]
    fn collisions_receive_suffix_without_overwriting() {
        let root = temp_root("collision");
        let target_dir = root.join("Album Artist").join("2026 - Album");
        std::fs::create_dir_all(&target_dir).unwrap();
        std::fs::write(target_dir.join("01 - Same [ytid0000001].m4a"), b"old").unwrap();
        let session = session(vec![
            row(1, "Same", "/tmp/inbox/first.m4a"),
            row(1, "Same", "/tmp/inbox/second.m4a"),
        ]);

        let plan = build_import_organize_plan(&session, &ImportOrganizeOptions::new(root.clone()))
            .unwrap();

        assert_eq!(
            plan.rows[0].target_path.as_deref(),
            Some(
                root.join("Album Artist")
                    .join("2026 - Album")
                    .join("01 - Same [ytid0000001] (2).m4a")
                    .as_path()
            )
        );
        assert_eq!(
            plan.rows[1].target_path.as_deref(),
            Some(
                root.join("Album Artist")
                    .join("2026 - Album")
                    .join("01 - Same [ytid0000001] (3).m4a")
                    .as_path()
            )
        );
        assert!(plan.rows[0].warnings[0].contains("collision"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn hostile_metadata_cannot_escape_root() {
        let root = temp_root("hostile");
        let mut bad = row(1, "../Song", "/tmp/inbox/Song.m4a");
        bad.album_artists = vec!["../../Artist".to_owned()];
        bad.album = Some("Album/../../Escape".to_owned());
        let session = session(vec![bad]);
        let plan = build_import_organize_plan(&session, &ImportOrganizeOptions::new(root.clone()))
            .unwrap();

        let target = plan.rows[0].target_path.as_ref().unwrap();
        assert!(target.starts_with(&root));
        assert!(!target.to_string_lossy().contains(".."));
    }

    #[test]
    fn rejected_or_missing_rows_are_not_planned_for_move() {
        let mut rejected = row(1, "Rejected", "/tmp/inbox/Rejected.m4a");
        rejected.review_decision = Some(ReviewDecision::Rejected);
        let mut missing = row(2, "Missing", "/tmp/inbox/Missing.m4a");
        missing.local_path = None;
        let session = session(vec![rejected, missing]);
        let plan =
            build_import_organize_plan(&session, &ImportOrganizeOptions::new(temp_root("skipped")))
                .unwrap();

        assert_eq!(plan.move_count, 0);
        assert_eq!(plan.skipped_count, 2);
        assert_eq!(plan.rows[0].decision, ImportOrganizeDecision::NotAccepted);
        assert_eq!(
            plan.rows[1].decision,
            ImportOrganizeDecision::MissingLocalPath
        );
    }

    #[test]
    fn apply_moves_audio_sidecar_and_updates_session() {
        let root = temp_root("apply");
        let inbox = private_import_session_dir(&root, "sp2yt-organize-apply");
        let audio = inbox.join("Song.m4a");
        std::fs::write(&audio, b"audio").unwrap();
        let sidecar = crate::downloads::sidecar_path(&audio);
        std::fs::write(&sidecar, b"{}").unwrap();
        let session = ImportSession {
            session_id: "sp2yt-organize-apply".to_owned(),
            rows: vec![row(1, "Song", &audio.to_string_lossy())],
            ..ImportSession::default()
        };
        session.save().unwrap();
        let plan = build_import_organize_plan(&session, &ImportOrganizeOptions::new(root.clone()))
            .unwrap();
        let target = plan.rows[0].target_path.clone().unwrap();

        let report = apply_import_organize_plan(&plan).unwrap();

        assert_eq!(report.moved_count, 1);
        assert!(!audio.exists());
        assert!(!sidecar.exists());
        assert!(target.exists());
        assert!(crate::downloads::sidecar_path(&target).exists());
        let saved = ImportSession::load("sp2yt-organize-apply").unwrap();
        assert_eq!(saved.rows[0].local_path.as_deref(), Some(target.as_path()));
        let retry = apply_import_organize_plan(&plan).unwrap();
        assert_eq!(retry.moved_count, 0);
        assert_eq!(retry.already_count, 1);
        assert_eq!(
            std::fs::read_dir(target.parent().unwrap())
                .unwrap()
                .filter_map(Result::ok)
                .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_file()))
                .count(),
            2
        );
        let _ = ImportSession::delete_record("sp2yt-organize-apply");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn apply_rejects_target_race_without_moving_source() {
        let root = temp_root("apply-race");
        let inbox = private_import_session_dir(&root, "sp2yt-organize-race");
        let audio = inbox.join("Song.m4a");
        std::fs::write(&audio, b"audio").unwrap();
        let sidecar = crate::downloads::sidecar_path(&audio);
        std::fs::write(&sidecar, b"{}").unwrap();
        let session = ImportSession {
            session_id: "sp2yt-organize-race".to_owned(),
            rows: vec![row(1, "Song", &audio.to_string_lossy())],
            ..ImportSession::default()
        };
        session.save().unwrap();
        let plan = build_import_organize_plan(&session, &ImportOrganizeOptions::new(root.clone()))
            .unwrap();
        let target = plan.rows[0].target_path.clone().unwrap();
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::write(&target, b"existing").unwrap();

        let err = apply_import_organize_plan(&plan).unwrap_err();

        assert!(format!("{err:#}").contains("artifact conflict"));
        assert!(audio.exists());
        assert!(sidecar.exists());
        assert_eq!(std::fs::read(&target).unwrap(), b"existing");

        std::fs::remove_file(&target).unwrap();
        let retry = apply_import_organize_plan(&plan).unwrap();
        assert_eq!(retry.moved_count, 0);
        assert_eq!(retry.already_count, 1);
        assert!(!audio.exists());
        assert!(!sidecar.exists());
        assert_eq!(std::fs::read(&target).unwrap(), b"audio");
        assert_eq!(
            std::fs::read(crate::downloads::sidecar_path(&target)).unwrap(),
            b"{}"
        );
        let _ = ImportSession::delete_record("sp2yt-organize-race");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn apply_does_not_move_files_after_import_record_was_deleted() {
        let root = temp_root("apply-deleted-record");
        let inbox = private_import_session_dir(&root, "sp2yt-organize-deleted-record");
        let audio = inbox.join("Song.m4a");
        std::fs::write(&audio, b"audio").unwrap();
        let session = ImportSession {
            session_id: "sp2yt-organize-deleted-record".to_owned(),
            rows: vec![row(1, "Song", &audio.to_string_lossy())],
            ..ImportSession::default()
        };
        let plan = build_import_organize_plan(&session, &ImportOrganizeOptions::new(root.clone()))
            .unwrap();
        let target = plan.rows[0].target_path.clone().unwrap();

        let error = apply_import_organize_plan(&plan)
            .expect_err("an already-deleted import record must abort before moving audio");

        assert!(error.to_string().contains("reload import session"));
        assert!(audio.exists());
        assert!(!target.exists());
        let _ = std::fs::remove_dir_all(root);
    }
}
