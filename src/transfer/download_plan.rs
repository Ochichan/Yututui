//! Session-aware import download planning.

use std::collections::HashMap;
use std::path::PathBuf;

use unicode_normalization::UnicodeNormalization;

use super::checkpoint::ReviewDecision;
use super::session::{ImportSession, ImportSessionRow, ImportSessionRowStatus};
use crate::api::Song;
use crate::downloads::DownloadStore;
use crate::local::{LocalIndex, LocalTrack};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportDownloadPlan {
    pub session_id: String,
    pub rows: Vec<ImportDownloadPlanRow>,
    pub enqueue_count: u32,
    pub linked_existing_count: u32,
    pub duplicate_count: u32,
    pub skipped_count: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportDownloadPlanRow {
    pub row_id: String,
    pub source_order: u32,
    pub title: String,
    pub selected_key: Option<String>,
    pub decision: ImportDownloadDecision,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImportDownloadDecision {
    Enqueue,
    AlreadyWritten { path: Option<PathBuf> },
    AlreadyDownloaded { path: Option<PathBuf> },
    AlreadyInLocalDeck { path: PathBuf },
    DuplicateInSession { first_source_order: u32 },
    NotAccepted,
    NoSelectedKey,
}

#[derive(Debug, Default, Clone)]
pub struct ImportDownloadDedupeIndex {
    downloaded: ExistingRows,
    local: ExistingRows,
}

impl ImportDownloadDedupeIndex {
    pub fn from_download_store(store: &DownloadStore) -> Self {
        let mut index = Self::default();
        for song in store.tracks_with_existing_files() {
            index.add_downloaded_song(song);
        }
        index
    }

    pub fn add_downloaded_song(&mut self, song: &Song) {
        if song
            .local_path
            .as_deref()
            .is_some_and(crate::downloads::is_existing_manifest_artifact)
        {
            self.downloaded.insert_song(song);
        }
    }

    pub fn add_local_index(&mut self, index: &LocalIndex) {
        for track in index.tracks() {
            self.add_local_track(track);
        }
    }

    pub fn add_local_track(&mut self, track: &LocalTrack) {
        self.local.insert_local_track(track);
    }
}

pub fn build_import_download_plan(
    session: &ImportSession,
    existing: &ImportDownloadDedupeIndex,
) -> ImportDownloadPlan {
    let mut seen = HashMap::<String, u32>::new();
    let mut rows = Vec::with_capacity(session.rows.len());
    let mut enqueue_count = 0u32;
    let mut linked_existing_count = 0u32;
    let mut duplicate_count = 0u32;
    let mut skipped_count = 0u32;

    for row in &session.rows {
        let selected_key = selected_key(row).map(str::to_owned);
        let decision = plan_row(row, selected_key.as_deref(), existing, &mut seen);
        match decision {
            ImportDownloadDecision::Enqueue => enqueue_count += 1,
            ImportDownloadDecision::AlreadyDownloaded { .. }
            | ImportDownloadDecision::AlreadyInLocalDeck { .. }
            | ImportDownloadDecision::AlreadyWritten { .. } => linked_existing_count += 1,
            ImportDownloadDecision::DuplicateInSession { .. } => duplicate_count += 1,
            ImportDownloadDecision::NotAccepted | ImportDownloadDecision::NoSelectedKey => {
                skipped_count += 1;
            }
        }
        rows.push(ImportDownloadPlanRow {
            row_id: row.row_id.clone(),
            source_order: row.source_order,
            title: row.title.clone(),
            selected_key,
            decision,
        });
    }

    ImportDownloadPlan {
        session_id: session.session_id.clone(),
        rows,
        enqueue_count,
        linked_existing_count,
        duplicate_count,
        skipped_count,
    }
}

fn plan_row(
    row: &ImportSessionRow,
    selected_key: Option<&str>,
    existing: &ImportDownloadDedupeIndex,
    seen: &mut HashMap<String, u32>,
) -> ImportDownloadDecision {
    if row.written || row.local_path.is_some() {
        return ImportDownloadDecision::AlreadyWritten {
            path: row.local_path.clone(),
        };
    }
    if !is_accepted_row(row) {
        return ImportDownloadDecision::NotAccepted;
    }
    let Some(selected_key) = selected_key.filter(|key| !key.trim().is_empty()) else {
        return ImportDownloadDecision::NoSelectedKey;
    };

    for key in dedupe_keys(row, selected_key) {
        if let Some(first_source_order) = seen.get(&key) {
            return ImportDownloadDecision::DuplicateInSession {
                first_source_order: *first_source_order,
            };
        }
    }

    if let Some(path) = existing.downloaded.match_row(row, selected_key) {
        remember_row_keys(row, selected_key, seen);
        return ImportDownloadDecision::AlreadyDownloaded { path };
    }
    if let Some(path) = existing.local.match_row(row, selected_key).flatten() {
        remember_row_keys(row, selected_key, seen);
        return ImportDownloadDecision::AlreadyInLocalDeck { path };
    }

    remember_row_keys(row, selected_key, seen);
    ImportDownloadDecision::Enqueue
}

fn is_accepted_row(row: &ImportSessionRow) -> bool {
    matches!(row.status, ImportSessionRowStatus::Matched)
        && !matches!(
            row.review_decision,
            Some(ReviewDecision::Rejected | ReviewDecision::Skipped)
        )
}

fn selected_key(row: &ImportSessionRow) -> Option<&str> {
    match &row.review_decision {
        Some(ReviewDecision::Accepted { key, .. }) => Some(key.as_str()),
        _ => row.selected_key.as_deref(),
    }
}

fn remember_row_keys(row: &ImportSessionRow, selected_key: &str, seen: &mut HashMap<String, u32>) {
    for key in dedupe_keys(row, selected_key) {
        seen.entry(key).or_insert(row.source_order);
    }
}

fn dedupe_keys(row: &ImportSessionRow, selected_key: &str) -> Vec<String> {
    let mut keys = Vec::new();
    push_key(&mut keys, "yt", selected_key);
    push_key(&mut keys, "src", &row.source_key);
    if let Some(isrc) = &row.isrc {
        push_key(&mut keys, "isrc", isrc);
    }
    if let Some(fingerprint) = row_fingerprint(row) {
        keys.push(format!("fp:{fingerprint}"));
    }
    keys
}

fn row_fingerprint(row: &ImportSessionRow) -> Option<String> {
    let artist = row.artists.first()?.trim();
    if row.title.trim().is_empty() || artist.is_empty() {
        return None;
    }
    Some(format!(
        "{}|{}|{}",
        normalize_key(&row.title),
        normalize_key(artist),
        row.duration_secs.unwrap_or_default()
    ))
}

fn push_key(keys: &mut Vec<String>, prefix: &str, raw: &str) {
    let normalized = normalize_key(raw);
    if !normalized.is_empty() {
        keys.push(format!("{prefix}:{normalized}"));
    }
}

fn normalize_key(raw: &str) -> String {
    raw.nfkc()
        .flat_map(char::to_lowercase)
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

#[derive(Debug, Default, Clone)]
struct ExistingRows {
    youtube: HashMap<String, Option<PathBuf>>,
    source_key: HashMap<String, Option<PathBuf>>,
    isrc: HashMap<String, Option<PathBuf>>,
    fingerprint: HashMap<String, Option<PathBuf>>,
}

impl ExistingRows {
    fn insert_song(&mut self, song: &Song) {
        let path = song.local_path.clone();
        if let Some(yt) = song.youtube_id() {
            self.youtube.insert(normalize_key(yt), path.clone());
        }
        if let Some(origin_key) = &song.origin_key {
            self.source_key
                .insert(normalize_key(origin_key), path.clone());
        }
        if let Some(isrc) = &song.isrc {
            self.isrc.insert(normalize_key(isrc), path.clone());
        }
        if let Some(fingerprint) = song_fingerprint(song) {
            self.fingerprint.insert(fingerprint, path);
        }
    }

    fn insert_local_track(&mut self, track: &LocalTrack) {
        let path = Some(track.path.clone());
        if let Some(yt) = &track.linked_video_id {
            self.youtube.insert(normalize_key(yt), path.clone());
        }
        if let Some(origin_key) = &track.origin_key {
            self.source_key
                .insert(normalize_key(origin_key), path.clone());
        }
        if let Some(isrc) = &track.isrc {
            self.isrc.insert(normalize_key(isrc), path.clone());
        }
        if let Some(fingerprint) = local_track_fingerprint(track) {
            self.fingerprint.insert(fingerprint, path);
        }
    }

    fn match_row(&self, row: &ImportSessionRow, selected_key: &str) -> Option<Option<PathBuf>> {
        let key = normalize_key(selected_key);
        if let Some(path) = self.youtube.get(&key) {
            return Some(path.clone());
        }
        let key = normalize_key(&row.source_key);
        if let Some(path) = self.source_key.get(&key) {
            return Some(path.clone());
        }
        if let Some(isrc) = &row.isrc {
            let key = normalize_key(isrc);
            if let Some(path) = self.isrc.get(&key) {
                return Some(path.clone());
            }
        }
        if let Some(fingerprint) = row_fingerprint(row)
            && let Some(path) = self.fingerprint.get(&fingerprint)
        {
            return Some(path.clone());
        }
        None
    }
}

fn song_fingerprint(song: &Song) -> Option<String> {
    let artist = song.artist.trim();
    if song.title.trim().is_empty() || artist.is_empty() {
        return None;
    }
    Some(format!(
        "{}|{}|{}",
        normalize_key(&song.title),
        normalize_key(artist),
        song.duration_secs.unwrap_or_default()
    ))
}

fn local_track_fingerprint(track: &LocalTrack) -> Option<String> {
    let artist = track.artist.first()?.trim();
    if track.title.trim().is_empty() || artist.is_empty() {
        return None;
    }
    Some(format!(
        "{}|{}|{}",
        normalize_key(&track.title),
        normalize_key(artist),
        track
            .duration_ms
            .map(|ms| (ms / 1000) as u32)
            .unwrap_or_default()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::local::{FileFingerprint, LocalTrack, LocalTrackId};
    use crate::transfer::session::{ImportSessionCounts, SessionEndpoint};

    fn session(rows: Vec<ImportSessionRow>) -> ImportSession {
        let counts = ImportSessionCounts {
            total: rows.len() as u32,
            matched: rows
                .iter()
                .filter(|row| matches!(row.status, ImportSessionRowStatus::Matched))
                .count() as u32,
            ..ImportSessionCounts::default()
        };
        ImportSession {
            schema_version: 1,
            session_id: "sp2yt-plan".to_owned(),
            session_instance_id: "test-download-plan-instance".to_owned(),
            job_id: "sp2yt-plan".to_owned(),
            created_at: 0,
            updated_at: 0,
            stage: crate::transfer::Stage::Writing,
            source: SessionEndpoint::default(),
            destination: SessionEndpoint::default(),
            counts,
            defer_reason: None,
            rows,
        }
    }

    fn row(order: u32, title: &str, selected_key: Option<&str>) -> ImportSessionRow {
        ImportSessionRow {
            row_id: format!("row-{order:05}"),
            source_order: order,
            status: ImportSessionRowStatus::Matched,
            title: title.to_owned(),
            artists: vec!["Artist".to_owned()],
            duration_secs: Some(180),
            source_key: format!("spotify:track:{order}"),
            selected_key: selected_key.map(str::to_owned),
            ..ImportSessionRow::default()
        }
    }

    #[test]
    fn plan_enqueues_accepted_rows_and_skips_review_rejections() {
        let mut rejected = row(2, "Rejected", Some("vid-b"));
        rejected.review_decision = Some(ReviewDecision::Rejected);
        let mut missing = row(3, "Missing", None);
        missing.status = ImportSessionRowStatus::NotFound;
        let plan = build_import_download_plan(
            &session(vec![row(1, "Accepted", Some("vid-a")), rejected, missing]),
            &ImportDownloadDedupeIndex::default(),
        );

        assert_eq!(plan.enqueue_count, 1);
        assert_eq!(plan.skipped_count, 2);
        assert_eq!(plan.rows[0].decision, ImportDownloadDecision::Enqueue);
        assert_eq!(plan.rows[1].decision, ImportDownloadDecision::NotAccepted);
        assert_eq!(plan.rows[2].decision, ImportDownloadDecision::NotAccepted);
    }

    #[test]
    fn plan_links_existing_download_store_rows() {
        let root = std::env::temp_dir().join(format!(
            "yututui-download-plan-existing-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let audio = root.join("Accepted.m4a");
        std::fs::write(&audio, b"audio").unwrap();
        let mut store = DownloadStore::default();
        store.record(
            &Song::remote("vid-a", "Accepted", "Artist", "3:00").with_local_path(audio.clone()),
        );
        let existing = ImportDownloadDedupeIndex::from_download_store(&store);
        let plan = build_import_download_plan(
            &session(vec![row(1, "Accepted", Some("vid-a"))]),
            &existing,
        );

        assert_eq!(plan.enqueue_count, 0);
        assert_eq!(plan.linked_existing_count, 1);
        assert_eq!(
            plan.rows[0].decision,
            ImportDownloadDecision::AlreadyDownloaded { path: Some(audio) }
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn plan_ignores_a_download_store_row_after_its_artifact_was_unlinked() {
        let mut store = DownloadStore::default();
        store.record(
            &Song::remote("vid-a", "Accepted", "Artist", "3:00")
                .with_local_path(PathBuf::from("/definitely/missing/Accepted.m4a")),
        );
        let existing = ImportDownloadDedupeIndex::from_download_store(&store);

        let plan = build_import_download_plan(
            &session(vec![row(1, "Accepted", Some("vid-a"))]),
            &existing,
        );

        assert_eq!(plan.enqueue_count, 1);
        assert_eq!(plan.rows[0].decision, ImportDownloadDecision::Enqueue);
    }

    #[test]
    fn plan_marks_later_session_duplicates() {
        let mut second = row(2, "Same", Some("vid-b"));
        second.source_key = "spotify:track:1".to_owned();
        let plan = build_import_download_plan(
            &session(vec![row(1, "Same", Some("vid-a")), second]),
            &ImportDownloadDedupeIndex::default(),
        );

        assert_eq!(plan.enqueue_count, 1);
        assert_eq!(plan.duplicate_count, 1);
        assert_eq!(
            plan.rows[1].decision,
            ImportDownloadDecision::DuplicateInSession {
                first_source_order: 1
            }
        );
    }

    #[test]
    fn plan_links_existing_local_deck_tracks_by_isrc() {
        let mut local = LocalTrack::untagged(PathBuf::from("/music/Accepted.m4a"), 100, 10);
        local.id =
            LocalTrackId::from_fingerprint(&FileFingerprint::path_mtime_size(&local.path, 10, 100));
        local.title = "Accepted".to_owned();
        local.artist = vec!["Artist".to_owned()];
        local.isrc = Some("ISRC123".to_owned());
        let mut existing = ImportDownloadDedupeIndex::default();
        existing.add_local_track(&local);
        let mut import_row = row(1, "Accepted", Some("vid-a"));
        import_row.isrc = Some("ISRC123".to_owned());

        let plan = build_import_download_plan(&session(vec![import_row]), &existing);

        assert_eq!(plan.linked_existing_count, 1);
        assert_eq!(
            plan.rows[0].decision,
            ImportDownloadDecision::AlreadyInLocalDeck {
                path: PathBuf::from("/music/Accepted.m4a")
            }
        );
    }
}
