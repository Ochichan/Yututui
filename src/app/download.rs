//! Download reducer methods, split out of the monolithic `app.rs` (behaviour-preserving).

use super::*;
use std::collections::HashSet;

use crate::transfer::organize_plan::{
    ImportOrganizeDecision, apply_import_organize_plan, build_import_organize_plan,
};

/// Ceiling on in-flight (dispatched-but-unfinished) downloads. Held comfortably below the
/// bounded command channel (`backpressure::DOWNLOAD_QUEUE` = 128) so a bulk batch drains
/// through `Downloads::pending` instead of overflowing the actor's `try_send`.
const BULK_INFLIGHT_CAP: usize = 96;

/// Hard ceiling on the `Downloads::pending` backlog. The bulk confirm popup and playlist
/// caps keep normal batches far below this; the bound just stops an unexpectedly huge batch
/// (or a future caller) from growing the queue without limit. Overflow remains in the confirm
/// popup for an explicit retry instead of being discarded.
const DOWNLOAD_PENDING_MAX: usize = 999;

impl App {
    /// Mark a single download as starting and emit the effect to run it. Routes through the
    /// same pending/pump path as bulk downloads; a lone song dispatches immediately since the
    /// in-flight cap dwarfs 1, so the visible behavior is unchanged.
    pub(in crate::app) fn start_download(&mut self, song: Song) -> Vec<Cmd> {
        if song.is_local() {
            self.status.text = format!(
                "{}: {}",
                t!("Already local", "이미 로컬에 있음"),
                song.title
            );
            self.dirty = true;
            return Vec::new();
        }
        self.status.text = format!(
            "{}: {} — {}",
            t!("Downloading", "다운로드 중"),
            song.title,
            song.artist
        );
        self.downloads.pending.push_back(song);
        self.dirty = true;
        self.pump_downloads()
    }

    /// Filter a batch down to the songs actually worth fetching: drop local files, drop tracks
    /// already downloaded in a past session (matched by YouTube id against the manifest and the
    /// current on-disk scan), and collapse duplicate ids within the batch. Preserves order.
    pub(in crate::app) fn downloadable_batch(&self, songs: Vec<Song>) -> Vec<Song> {
        let mut seen: HashSet<String> = HashSet::new();
        songs
            .into_iter()
            .filter(|song| {
                // Local files are already on disk; radio stations are live streams, not
                // downloadable tracks — neither belongs in a bulk fetch.
                if song.is_local() || song.is_radio_station() {
                    return false;
                }
                if let Some(yt) = song.youtube_id()
                    && (self.download_store.contains_youtube_id(yt)
                        || self.library_ui.downloaded.iter().any(|downloaded| {
                            downloaded.youtube_id() == Some(yt)
                                && downloaded
                                    .local_path
                                    .as_deref()
                                    .is_some_and(crate::downloads::is_existing_manifest_artifact)
                        }))
                {
                    return false; // already downloaded before
                }
                let key = song
                    .youtube_id()
                    .unwrap_or(song.video_id.as_str())
                    .to_string();
                seen.insert(key) // false when this id already appeared in the batch
            })
            .collect()
    }

    /// Compute the real downloadable batch for `songs` and, if any remain, raise the bulk
    /// confirm popup ("Download N songs?"). An empty batch (everything local/already fetched)
    /// just surfaces a toast — no modal.
    pub(in crate::app) fn open_confirm_download(&mut self, songs: Vec<Song>) -> Vec<Cmd> {
        let batch = self.downloadable_batch(songs);
        self.dirty = true;
        if batch.is_empty() {
            self.status.kind = StatusKind::Info;
            self.status.text = t!("Nothing to download", "다운로드할 곡이 없음").to_string();
            return Vec::new();
        }
        self.library_ui.confirm_download = Some(batch);
        Vec::new()
    }

    /// Confirm the bulk-download popup: queue the stored (already-deduped) batch and start
    /// draining it into the actor.
    pub(in crate::app) fn confirm_download_apply(&mut self) -> Vec<Cmd> {
        let Some(batch) = self.library_ui.confirm_download.take() else {
            return Vec::new();
        };
        // Bound the pending backlog (defensive; normal batches are far smaller). Preserve any
        // overflow in the confirmation popup so the user can retry after the backlog drains.
        let free = DOWNLOAD_PENDING_MAX.saturating_sub(self.downloads.pending.len());
        let mut batch = batch.into_iter();
        let accepted: Vec<Song> = batch.by_ref().take(free).collect();
        let deferred: Vec<Song> = batch.collect();
        let accepted_count = accepted.len();
        let deferred_count = deferred.len();
        if deferred.is_empty() {
            self.status.kind = StatusKind::Info;
            self.status.text = format!(
                "{}: {accepted_count}",
                t!("Queued for download", "다운로드 대기")
            );
        } else {
            self.library_ui.confirm_download = Some(deferred);
            self.status.kind = StatusKind::Error;
            self.status.text = format!(
                "{}: {accepted_count}; {}: {deferred_count}",
                t!("Queued for download", "다운로드 대기"),
                t!("Waiting for retry", "재시도 대기")
            );
        }
        self.downloads.pending.extend(accepted);
        self.dirty = true;
        self.pump_downloads()
    }

    /// Hand pending downloads to the actor up to [`BULK_INFLIGHT_CAP`], marking each `Running`
    /// so its indicator shows at once. Returns one `DownloadCmd::Start` per dispatched song. Called
    /// on every enqueue and again as each download finishes, so the queue keeps flowing without
    /// ever exceeding the channel bound.
    pub(in crate::app) fn pump_downloads(&mut self) -> Vec<Cmd> {
        let missing = self
            .runtime_tool_checks
            .then(crate::deps::missing_for_downloads)
            .unwrap_or_default();
        if self.block_downloads_for_missing(missing) {
            return Vec::new();
        }
        let mut cmds = Vec::new();
        while self.downloads.dispatched < BULK_INFLIGHT_CAP {
            let Some(song) = self.downloads.pending.pop_front() else {
                break;
            };
            // Ordinary downloads dedupe by video id. Import work is owned by session row, so two
            // sessions selecting the same video remain independent and each gets a completion.
            let tracking_key = crate::download::download_tracking_key(&song);
            if matches!(
                self.downloads.active.get(&tracking_key),
                Some(DownloadState::Running(_))
            ) {
                continue;
            }
            self.downloads
                .active
                .insert(tracking_key.clone(), DownloadState::Running(0));
            if let (Some(session_id), Some(source_order)) =
                (song.import_session_id.as_ref(), song.import_source_order)
            {
                self.downloads
                    .auto_organize_rows
                    .entry(session_id.clone())
                    .or_default()
                    .insert(source_order);
            }
            self.downloads.sources.insert(tracking_key, song.clone());
            self.downloads.dispatched += 1;
            cmds.push(Cmd::Download(DownloadCmd::Start(Box::new(song))));
        }
        cmds
    }

    pub(in crate::app) fn block_downloads_for_missing(
        &mut self,
        missing: Vec<&'static str>,
    ) -> bool {
        if self.downloads.pending.is_empty() || missing.is_empty() {
            return false;
        }
        self.show_tool_setup(ToolSetupContext::Downloads, missing);
        true
    }

    pub(in crate::app) fn apply_download_progress(&mut self, tracking_key: String, percent: f64) {
        if matches!(
            self.downloads.active.get(&tracking_key),
            Some(DownloadState::Done | DownloadState::Failed)
        ) {
            return;
        }
        let percent = percent.round() as u8;
        let changed = !matches!(
            self.downloads.active.get(&tracking_key),
            Some(DownloadState::Running(previous)) if *previous == percent
        );
        if changed {
            self.downloads
                .active
                .insert(tracking_key, DownloadState::Running(percent));
            self.dirty = true;
        }
    }

    pub(in crate::app) fn apply_download_done(
        &mut self,
        tracking_key: String,
        path: String,
    ) -> Vec<Cmd> {
        if matches!(
            self.downloads.active.get(&tracking_key),
            Some(DownloadState::Done | DownloadState::Failed)
        ) {
            return Vec::new();
        }
        self.downloads
            .active
            .insert(tracking_key.clone(), DownloadState::Done);
        self.downloads.dispatched = self.downloads.dispatched.saturating_sub(1);
        let saved = !path.trim().is_empty();
        let import_session_id = self
            .downloads
            .sources
            .get(&tracking_key)
            .and_then(|song| song.import_session_id.clone());
        // Preserve the ordinary empty-path terminal behavior; import ownership must still be
        // released so it cannot block the session's settled check forever.
        let source = if saved || import_session_id.is_some() {
            self.downloads.sources.remove(&tracking_key)
        } else {
            None
        };
        if saved {
            let path_buf = PathBuf::from(&path);
            let local = source
                .map(|source| source.with_local_path(path_buf.clone()))
                .unwrap_or_else(|| Song::local_file(path_buf));
            self.add_downloaded_track(local);
        }
        self.status.kind = StatusKind::Info;
        self.status.text = format!("{}: {path}", t!("Saved", "저장됨"));
        self.dirty = true;
        let mut commands = self.pump_downloads();
        if let Some(session_id) = import_session_id {
            commands.extend(self.auto_organize_settled_import(&session_id));
        }
        if saved
            && !commands
                .iter()
                .any(|cmd| matches!(cmd, Cmd::Persist(PersistCmd::Downloads)))
        {
            commands.push(Cmd::Persist(PersistCmd::Downloads));
        }
        commands
    }

    pub(in crate::app) fn apply_download_error(
        &mut self,
        tracking_key: String,
        error: String,
    ) -> Vec<Cmd> {
        if matches!(
            self.downloads.active.get(&tracking_key),
            Some(DownloadState::Done | DownloadState::Failed)
        ) {
            return Vec::new();
        }
        self.downloads
            .active
            .insert(tracking_key.clone(), DownloadState::Failed);
        let import_session_id = self
            .downloads
            .sources
            .remove(&tracking_key)
            .and_then(|song| song.import_session_id);
        self.downloads.dispatched = self.downloads.dispatched.saturating_sub(1);
        self.status.text = format!("{}: {error}", t!("Download failed", "다운로드 실패"));
        self.dirty = true;
        let mut commands = self.pump_downloads();
        if let Some(session_id) = import_session_id {
            commands.extend(self.auto_organize_settled_import(&session_id));
        }
        commands
    }

    /// Publish only the import rows admitted in this runtime batch once every queued/running
    /// download from the same session has reached a terminal state. A saved session may contain
    /// older inbox artifacts, so the full deterministic plan is filtered to the tracked source
    /// orders before it is applied.
    fn auto_organize_settled_import(&mut self, session_id: &str) -> Vec<Cmd> {
        let still_pending = self
            .downloads
            .pending
            .iter()
            .any(|song| song.import_session_id.as_deref() == Some(session_id));
        let still_running = self
            .downloads
            .sources
            .values()
            .any(|song| song.import_session_id.as_deref() == Some(session_id));
        if still_pending || still_running {
            return Vec::new();
        }

        let Some(source_orders) = self.downloads.auto_organize_rows.remove(session_id) else {
            return Vec::new();
        };
        let result: anyhow::Result<
            Option<crate::transfer::organize_plan::ImportOrganizeApplyReport>,
        > = (|| {
            let session = crate::transfer::session::ImportSession::load(session_id)?;
            let mut plan = build_import_organize_plan(&session, &self.import_organize_options())?;
            plan.rows
                .retain(|row| source_orders.contains(&row.source_order));
            plan.move_count = 0;
            plan.already_count = 0;
            plan.skipped_count = 0;
            for row in &plan.rows {
                match row.decision {
                    ImportOrganizeDecision::Move => plan.move_count += 1,
                    ImportOrganizeDecision::AlreadyAtTarget => plan.already_count += 1,
                    ImportOrganizeDecision::NotAccepted
                    | ImportOrganizeDecision::MissingLocalPath => plan.skipped_count += 1,
                }
            }
            if plan.move_count == 0 {
                return Ok(None);
            }
            let report = apply_import_organize_plan(&plan)?;
            Ok(Some(report))
        })();

        let paths_changed =
            self.refresh_organized_import_download_paths(session_id, &source_orders);
        let mut deferred_refresh = if paths_changed && !matches!(&result, Ok(Some(_))) {
            self.request_local_scan(false)
        } else {
            Vec::new()
        };
        let mut commands = match result {
            Ok(Some(report)) => {
                let commands = self.request_local_scan(false);
                self.status.kind = StatusKind::Info;
                self.status.text = format!(
                    "{} {session_id}: {} {}, {} {}, {} {}",
                    t!("Organized import session", "임포트 세션 정리됨"),
                    report.moved_count,
                    t!("moved", "이동됨"),
                    report.already_count,
                    t!("already", "이미 정리됨"),
                    report.skipped_count,
                    t!("failed; retry from Local Deck", "실패; 로컬 덱에서 재시도")
                );
                self.dirty = true;
                commands
            }
            Ok(None) => Vec::new(),
            Err(error) => {
                self.status.kind = StatusKind::Error;
                self.status.text = format!(
                    "{}: {error:#}. {}",
                    t!(
                        "Automatic import organize did not finish; artifacts remain recoverable",
                        "임포트 자동 정리가 완료되지 않음; 파일은 복구 가능한 상태로 유지됨"
                    ),
                    t!(
                        "Select the session in Local Deck and press m to retry",
                        "로컬 덱에서 세션을 선택하고 m을 눌러 재시도하세요"
                    )
                );
                self.dirty = true;
                Vec::new()
            }
        };
        deferred_refresh.append(&mut commands);
        let mut commands = deferred_refresh;
        if paths_changed {
            commands.push(Cmd::Persist(PersistCmd::Downloads));
        }
        commands
    }

    fn refresh_organized_import_download_paths(
        &mut self,
        session_id: &str,
        source_orders: &HashSet<u32>,
    ) -> bool {
        let Ok(session) = crate::transfer::session::ImportSession::load(session_id) else {
            return false;
        };
        let paths: std::collections::HashMap<_, _> = session
            .rows
            .into_iter()
            .filter(|row| source_orders.contains(&row.source_order))
            .filter_map(|row| row.local_path.map(|path| (row.source_order, path)))
            .collect();
        if paths.is_empty() {
            return false;
        }

        let manifest_changed = self
            .download_store
            .relocate_import_paths(session_id, &paths);
        let mut runtime_changed = false;
        for song in &mut self.library_ui.downloaded {
            if song.import_session_id.as_deref() != Some(session_id) {
                continue;
            }
            let Some(path) = song
                .import_source_order
                .and_then(|source_order| paths.get(&source_order))
            else {
                continue;
            };
            if song.local_path.as_ref() != Some(path) {
                *song = song.with_local_path(path.clone());
                runtime_changed = true;
            }
        }
        if runtime_changed {
            let mut seen = HashSet::new();
            self.library_ui
                .downloaded
                .retain(|song| seen.insert(song.video_id.clone()));
            self.library_ui.downloaded_rev = self.library_ui.downloaded_rev.wrapping_add(1);
        }
        manifest_changed || runtime_changed
    }

    pub(in crate::app) fn add_downloaded_track(&mut self, song: Song) {
        // Remember the enriched record (id + real metadata) BEFORE the move below — `insert`
        // takes `song` by value, so recording after it would not compile.
        self.download_store.record(&song);
        self.library_ui.downloaded_rev = self.library_ui.downloaded_rev.wrapping_add(1);
        self.library_ui
            .downloaded
            .retain(|s| s.video_id != song.video_id);
        self.library_ui.downloaded.insert(0, song);
        self.library_ui.downloaded.truncate(DOWNLOADED_TRACKS_MAX);
    }

    /// Turn a bare disk scan into the Downloads-tab list, restoring each track's YouTube
    /// identity where possible: first from the persisted manifest (by `video_id` — brings back
    /// the real artist/duration too), then, for anything still id-less (e.g. files downloaded
    /// before the id-in-filename scheme), a best-effort normalized-title match against remote
    /// favorites/history entries. Files with no recoverable origin stay local-only (correct).
    pub fn enrich_downloads(&self, scanned: Vec<Song>) -> Vec<Song> {
        self.download_store
            .enrich(scanned)
            .into_iter()
            .map(|song| match self.legacy_youtube_id_for(&song) {
                Some(id) => song.with_yt_id(id),
                None => song,
            })
            .collect()
    }

    /// Best-effort recovery of a YouTube id for an id-less local `song` by matching its
    /// normalized title against a *remote* favorite/history entry (one that actually has a
    /// YouTube origin). Returns `None` when the song already has an id or nothing matches.
    fn legacy_youtube_id_for(&self, song: &Song) -> Option<String> {
        if song.youtube_id().is_some() {
            return None;
        }
        let key = song.title.trim().to_lowercase();
        self.library
            .favorites
            .iter()
            .chain(self.library.history.iter())
            .find(|e| e.youtube_id().is_some() && e.title.trim().to_lowercase() == key)
            .and_then(|e| e.youtube_id().map(str::to_owned))
    }
}
