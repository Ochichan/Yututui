//! Daemon owner-lane host for the transfer actor and retained `transfer` projection.

use std::collections::VecDeque;

use serde::Deserialize;

use crate::remote::proto::{
    RemoteCommand, RemoteResponse, SpotifyPlaylistModel, TransferJobModel, TransferPhaseModel,
    TransferReportModel,
};
use crate::remote::publish::Publisher;
use crate::remote::server::RemoteEvent;
use crate::transfer::actor::{PickerPlaylist, TransferCmd, TransferEvent, TransferHandle};
use crate::transfer::checkpoint::TransferReport;
use crate::transfer::{
    ImportMediaKind, JobSpec, MatchPolicy, TransferCacheMode, TransferDest, TransferSource,
};

use super::engine::DaemonEngine;
use super::events::{DaemonEvent, DaemonEventSender, emit_daemon_event};

const LIKED_SOURCE_ID: &str = "spotify:liked";

pub(super) struct TransferHost {
    handle: TransferHandle,
    state: TransferState,
    pending: VecDeque<JobSpec>,
}

impl TransferHost {
    pub(super) fn spawn(event_tx: DaemonEventSender) -> Self {
        let handle = crate::transfer::actor::spawn(move |event| {
            emit_daemon_event(&event_tx, DaemonEvent::Transfer(event))
        });
        Self {
            handle,
            state: TransferState::default(),
            pending: VecDeque::new(),
        }
    }

    pub(super) fn publish(&self, publisher: &mut Publisher) {
        self.state.publish(publisher);
    }

    /// Intercept owner-only transfer commands and actor events before engine dispatch.
    pub(super) fn intercept(
        &mut self,
        event: DaemonEvent,
        engine: &mut DaemonEngine,
        publisher: &mut Publisher,
    ) -> Option<DaemonEvent> {
        match event {
            DaemonEvent::Remote(RemoteEvent::Command(command, reply)) => {
                match self.command(engine, command) {
                    Ok((response, publish)) => {
                        let _ = reply.send(response);
                        if publish {
                            self.state.publish(publisher);
                        }
                        None
                    }
                    Err(command) => Some(DaemonEvent::Remote(RemoteEvent::Command(command, reply))),
                }
            }
            DaemonEvent::Remote(RemoteEvent::SessionCommand {
                command,
                origin,
                reply,
            }) => match self.command(engine, command) {
                Ok((response, publish)) => {
                    let _ = reply.send(response);
                    if publish {
                        self.state.publish(publisher);
                    }
                    None
                }
                Err(command) => Some(DaemonEvent::Remote(RemoteEvent::SessionCommand {
                    command,
                    origin,
                    reply,
                })),
            },
            event => Some(event),
        }
    }

    fn command(
        &mut self,
        engine: &DaemonEngine,
        command: RemoteCommand,
    ) -> Result<(RemoteResponse, bool), RemoteCommand> {
        match command {
            RemoteCommand::SpotifyConnect => {
                let (client_id, port) = engine.spotify_auth_config();
                let Some(client_id) = client_id.filter(|id| !id.trim().is_empty()) else {
                    return Ok((
                        RemoteResponse::err_with_message(
                            "bad_request",
                            "Configure a Spotify client ID before connecting".to_owned(),
                        ),
                        false,
                    ));
                };
                Ok((
                    send_response(
                        &self.handle,
                        TransferCmd::AuthStart { client_id, port },
                        "spotify auth started",
                    ),
                    false,
                ))
            }
            RemoteCommand::TransferListSpotify => {
                let response = send_response(
                    &self.handle,
                    TransferCmd::ListSpotifyPlaylists,
                    "spotify playlists requested",
                );
                if response.ok {
                    self.state.listing();
                }
                let publish = response.ok;
                Ok((response, publish))
            }
            RemoteCommand::TransferStart { spec } => {
                let mut jobs = match parse_jobs(engine, spec) {
                    Ok(jobs) => jobs,
                    Err((reason, message)) => {
                        return Ok((RemoteResponse::err_with_message(reason, message), false));
                    }
                };
                let first = jobs
                    .pop_front()
                    .expect("validated transfer jobs are non-empty");
                let dest = dest_label(&first.dest);
                let job_count = jobs.len() + 1;
                let response = send_response(
                    &self.handle,
                    TransferCmd::StartJob(Box::new(first)),
                    "transfer started",
                );
                if response.ok {
                    self.pending = jobs;
                    self.state.running(job_count, dest);
                }
                let publish = response.ok;
                Ok((response, publish))
            }
            RemoteCommand::TransferCancel => {
                let response = send_response(
                    &self.handle,
                    TransferCmd::CancelJob,
                    "transfer cancellation requested",
                );
                if response.ok {
                    self.pending.clear();
                }
                Ok((response, false))
            }
            command => Err(command),
        }
    }

    pub(super) fn on_event(
        &mut self,
        event: TransferEvent,
        engine: &mut DaemonEngine,
        publisher: &mut Publisher,
    ) {
        if apply_account_event(&event, engine) {
            return;
        }
        match event {
            TransferEvent::AuthUrl(url) => {
                publisher.publish_accounts_auth_url("spotify", &url);
                return;
            }
            TransferEvent::AuthDone { .. } => unreachable!("handled above"),
            TransferEvent::AuthError(error) => {
                let error = crate::util::sanitize::sanitize_error_text(error);
                tracing::warn!(%error, "spotify authorization failed");
                // One-shot failure signal so the GUI's connect flow can stop waiting
                // (the approval URL is one-shot too; there is nothing to retain).
                publisher.publish_accounts_auth_failed("spotify", &error);
                return;
            }
            TransferEvent::Disconnected => unreachable!("handled above"),
            TransferEvent::SpotifyPlaylists(Ok(items)) => self.state.ready(items),
            TransferEvent::SpotifyPlaylists(Err(error)) => self
                .state
                .failed(crate::util::sanitize::sanitize_error_text(error)),
            TransferEvent::Progress(progress) => self.state.progress(progress),
            TransferEvent::JobDone(report) => {
                // The import wrote the on-disk playlist store; refresh the engine's
                // in-memory copy so its next save cannot clobber the imported rows,
                // and so the `playlists` topic shows the result immediately.
                engine.reload_playlists_after_import();
                self.state.absorb_report(&report);
                if let Some(next) = self.pending.pop_front() {
                    if self
                        .handle
                        .send(TransferCmd::StartJob(Box::new(next)))
                        .is_ok()
                    {
                        self.state.next_job();
                    } else {
                        self.pending.clear();
                        self.state
                            .failed("transfer actor unavailable before queued job".to_owned());
                    }
                } else {
                    self.state.done();
                }
            }
            TransferEvent::JobRejected { job_id, error } => {
                self.pending.clear();
                self.state.failed(format!(
                    "job {job_id} rejected: {}",
                    crate::util::sanitize::sanitize_error_text(error)
                ));
            }
            TransferEvent::JobFailed {
                job_id,
                error,
                resumable,
            } => {
                self.pending.clear();
                if error == "cancelled" {
                    // A user cancel is not a failure: back to the picker (or idle when
                    // no sources are loaded), no error banner.
                    self.state.cancelled();
                } else {
                    let error = crate::util::sanitize::sanitize_error_text(error);
                    self.state.failed(if resumable && !job_id.is_empty() {
                        format!("{error} · resume with: ytt transfer resume {job_id}")
                    } else {
                        error
                    });
                }
            }
        }
        self.state.publish(publisher);
    }
}

fn apply_account_event(event: &TransferEvent, engine: &mut DaemonEngine) -> bool {
    match event {
        TransferEvent::AuthDone { display_name } => {
            engine.set_spotify_user(Some(display_name.clone()));
            true
        }
        TransferEvent::Disconnected => {
            engine.set_spotify_user(None);
            true
        }
        _ => false,
    }
}

fn send_response(handle: &TransferHandle, command: TransferCmd, message: &str) -> RemoteResponse {
    match handle.send(command) {
        Ok(_) => RemoteResponse::ok(message.to_owned()),
        Err(_) => RemoteResponse::err("busy"),
    }
}

#[derive(Debug, Deserialize)]
struct GuiTransferSpec {
    source_ids: Vec<String>,
    dest: GuiTransferDest,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum GuiTransferDest {
    New { name: String },
    Existing { playlist_id: String },
}

fn parse_jobs(
    engine: &DaemonEngine,
    value: serde_json::Value,
) -> Result<VecDeque<JobSpec>, (&'static str, String)> {
    let spec: GuiTransferSpec = serde_json::from_value(value)
        .map_err(|error| ("bad_request", format!("invalid transfer spec: {error}")))?;
    if spec.source_ids.is_empty() {
        return Err((
            "bad_request",
            "source_ids must contain at least one Spotify source".to_owned(),
        ));
    }
    // The wizard's destination picker lists the app's LOCAL playlists (the daemon
    // `playlists` topic), so both dest kinds land in the local store — create-or-append
    // by name, browsable immediately, no YouTube Music account writes and no cookie
    // requirement. (YTM-account destinations remain a CLI/TUI feature.)
    let dest_name = match spec.dest {
        GuiTransferDest::New { name } => {
            let name = name.trim().to_owned();
            if name.is_empty() {
                return Err((
                    "bad_request",
                    "new destination name must not be empty".to_owned(),
                ));
            }
            name
        }
        GuiTransferDest::Existing { playlist_id } => {
            let playlist_id = playlist_id.trim().to_owned();
            let Some(name) = engine.playlist_name(&playlist_id) else {
                return Err((
                    "unknown_playlist",
                    format!("no local playlist with id {playlist_id}"),
                ));
            };
            name
        }
    };
    let dest = TransferDest::LocalPlaylist {
        name: Some(dest_name),
    };
    spec.source_ids
        .into_iter()
        .map(|id| {
            let id = id.trim().to_owned();
            if id.is_empty() {
                return Err((
                    "bad_request",
                    "source_ids must not contain empty ids".to_owned(),
                ));
            }
            Ok(gui_job(source_from_id(id), dest.clone()))
        })
        .collect()
}

/// GUI imports use the TUI's default FastPlaylist settings.
fn gui_job(source: TransferSource, dest: TransferDest) -> JobSpec {
    JobSpec {
        source,
        dest,
        media_kind: ImportMediaKind::Track,
        dry_run: false,
        min_score: 0.80,
        take_best: false,
        auto_accept_ambiguous_min_score: Some(0.75),
        match_policy: MatchPolicy::Balanced,
        allow_user_videos: false,
        cache_mode: TransferCacheMode::Use,
        rematch: false,
    }
}

fn source_from_id(id: String) -> TransferSource {
    if id == LIKED_SOURCE_ID {
        TransferSource::SpotifyLiked
    } else {
        TransferSource::SpotifyPlaylist { id }
    }
}

fn source_model(item: PickerPlaylist) -> SpotifyPlaylistModel {
    let id = match item.source {
        TransferSource::SpotifyLiked => LIKED_SOURCE_ID.to_owned(),
        TransferSource::SpotifyPlaylist { id } => id,
        _ => {
            tracing::warn!("transfer actor returned a non-Spotify picker source");
            return SpotifyPlaylistModel {
                id: String::new(),
                name: item.label,
                count: u64::from(item.total),
            };
        }
    };
    SpotifyPlaylistModel {
        id,
        name: item.label,
        count: u64::from(item.total),
    }
}

fn dest_label(dest: &TransferDest) -> String {
    match dest {
        TransferDest::YtmNewPlaylist { name } => name
            .clone()
            .unwrap_or_else(|| "New YouTube Music playlist".to_owned()),
        TransferDest::YtmExistingPlaylist { name } => name.clone(),
        _ => "YouTube Music".to_owned(),
    }
}

#[derive(Default)]
struct TransferState {
    phase: TransferPhaseModel,
    sources: Vec<SpotifyPlaylistModel>,
    job: Option<TransferJobModel>,
    report: Option<TransferReportModel>,
    error: Option<String>,
    aggregate: ReportAggregate,
    job_count: usize,
    dest: String,
}

impl TransferState {
    fn publish(&self, publisher: &mut Publisher) {
        publisher.publish_transfer(
            self.phase,
            self.sources.clone(),
            self.job.clone(),
            self.report.clone(),
            self.error.clone(),
        );
    }

    fn listing(&mut self) {
        self.phase = TransferPhaseModel::Listing;
        self.error = None;
    }

    fn ready(&mut self, items: Vec<PickerPlaylist>) {
        self.sources = items.into_iter().map(source_model).collect();
        self.phase = TransferPhaseModel::Ready;
        self.job = None;
        self.report = None;
        self.error = None;
    }

    fn running(&mut self, job_count: usize, dest: String) {
        self.phase = TransferPhaseModel::Running;
        self.job = Some(TransferJobModel {
            done: 0,
            total: 0,
            matched: 0,
            failed: 0,
        });
        self.report = None;
        self.error = None;
        self.aggregate = ReportAggregate::default();
        self.job_count = job_count;
        self.dest = dest;
    }

    fn next_job(&mut self) {
        self.phase = TransferPhaseModel::Running;
        self.job = Some(TransferJobModel {
            done: 0,
            total: 0,
            matched: 0,
            failed: 0,
        });
    }

    fn progress(&mut self, progress: crate::transfer::TransferProgress) {
        self.phase = TransferPhaseModel::Running;
        self.job = Some(TransferJobModel {
            done: u64::from(progress.done),
            total: u64::from(progress.total),
            matched: u64::from(progress.matched),
            failed: u64::from(progress.ambiguous.saturating_add(progress.not_found)),
        });
        self.error = None;
    }

    fn absorb_report(&mut self, report: &TransferReport) {
        self.aggregate.absorb(report);
    }

    fn done(&mut self) {
        self.phase = TransferPhaseModel::Done;
        self.job = None;
        self.error = None;
        self.report = Some(self.aggregate.model(self.job_count, self.dest.clone()));
    }

    fn failed(&mut self, error: String) {
        self.phase = TransferPhaseModel::Failed;
        self.job = None;
        self.report = None;
        self.error = Some(error);
    }

    /// A user cancel: back to the picker (or idle with nothing listed) — not a failure.
    fn cancelled(&mut self) {
        self.phase = if self.sources.is_empty() {
            TransferPhaseModel::Idle
        } else {
            TransferPhaseModel::Ready
        };
        self.job = None;
        self.report = None;
        self.error = None;
    }
}

#[derive(Default)]
struct ReportAggregate {
    job_ids: Vec<String>,
    matched: u64,
    failed: u64,
    skipped: u64,
    unmatched: Vec<String>,
    overflow_unmatched: u64,
    has_review_rows: bool,
}

impl ReportAggregate {
    fn absorb(&mut self, report: &TransferReport) {
        self.job_ids.push(report.job_id.clone());
        self.matched += u64::from(report.matched);
        self.failed += (report.ambiguous.len()
            + report.not_found.len()
            + report.deferred.len()
            + report.capacity_skipped.len()) as u64;
        self.skipped += u64::from(
            report
                .skipped_local
                .saturating_add(report.duplicates_dropped),
        );
        self.has_review_rows |= !report.ambiguous.is_empty();
        // Bounded: a big import with many misses must not balloon the retained topic;
        // the overflow shows up as one summary row instead.
        const UNMATCHED_MAX: usize = 100;
        for title in report
            .ambiguous
            .iter()
            .chain(&report.not_found)
            .chain(&report.deferred)
            .chain(&report.capacity_skipped)
            .map(|row| row.title.as_str())
        {
            if self.unmatched.len() < UNMATCHED_MAX {
                self.unmatched.push(title.to_owned());
            } else {
                self.overflow_unmatched += 1;
            }
        }
    }

    fn model(&self, job_count: usize, dest: String) -> TransferReportModel {
        let job_id = (job_count == 1)
            .then(|| self.job_ids.first().cloned())
            .flatten();
        let review_command = job_id
            .as_ref()
            .filter(|_| self.has_review_rows)
            .map(|id| format!("ytt transfer review {id}"));
        let report_command = job_id
            .as_ref()
            .map(|id| format!("ytt transfer report {id}"));
        let mut unmatched = self.unmatched.clone();
        if self.overflow_unmatched > 0 {
            unmatched.push(format!("… and {} more", self.overflow_unmatched));
        }
        TransferReportModel {
            job_id,
            matched: self.matched,
            failed: self.failed,
            skipped: self.skipped,
            unmatched,
            dest,
            review_command,
            report_command,
            download_preview_command: None,
            organize_preview_command: None,
            local_deck_hint: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn engine() -> DaemonEngine {
        super::super::engine::tests::engine_with_queue(&[])
    }
    use crate::transfer::Stage;
    use crate::transfer::checkpoint::ReportRow;

    fn report(job_id: &str) -> TransferReport {
        TransferReport {
            job_id: job_id.to_owned(),
            matched: 7,
            ambiguous: vec![ReportRow {
                title: "Needs review".to_owned(),
                ..ReportRow::default()
            }],
            not_found: vec![ReportRow {
                title: "Missing".to_owned(),
                ..ReportRow::default()
            }],
            skipped_local: 2,
            duplicates_dropped: 1,
            ..TransferReport::default()
        }
    }

    #[test]
    fn listing_ready_running_done_maps_retained_models() {
        let mut state = TransferState::default();
        state.listing();
        assert_eq!(state.phase, TransferPhaseModel::Listing);
        state.ready(vec![PickerPlaylist {
            source: TransferSource::SpotifyPlaylist {
                id: "p1".to_owned(),
            },
            label: "Road trip".to_owned(),
            total: 4,
        }]);
        assert_eq!(state.phase, TransferPhaseModel::Ready);
        assert_eq!(state.sources[0].id, "p1");
        assert_eq!(state.sources[0].count, 4);

        state.running(1, "Destination".to_owned());
        state.progress(crate::transfer::TransferProgress {
            job_id: "job-1".to_owned(),
            stage: Stage::Matching,
            done: 5,
            total: 10,
            matched: 3,
            auto_accepted: 0,
            ambiguous: 1,
            not_found: 1,
            written: 0,
            current: "Track".to_owned(),
        });
        assert_eq!(state.job.as_ref().unwrap().failed, 2);
        state.absorb_report(&report("job-1"));
        state.done();

        assert_eq!(state.phase, TransferPhaseModel::Done);
        let model = state.report.unwrap();
        assert_eq!(model.job_id.as_deref(), Some("job-1"));
        assert_eq!(model.matched, 7);
        assert_eq!(model.failed, 2);
        assert_eq!(model.skipped, 3);
        assert_eq!(model.unmatched, ["Needs review", "Missing"]);
        assert_eq!(model.dest, "Destination");
        assert_eq!(
            model.review_command.as_deref(),
            Some("ytt transfer review job-1")
        );
        assert_eq!(
            model.report_command.as_deref(),
            Some("ytt transfer report job-1")
        );
    }

    #[test]
    fn multi_source_jobs_are_sequential_into_the_local_store() {
        let engine = engine();
        let jobs = parse_jobs(
            &engine,
            serde_json::json!({
                "source_ids": ["one", "two"],
                "dest": { "kind": "new", "name": "Mix" }
            }),
        )
        .unwrap();
        assert_eq!(jobs.len(), 2);
        // Both jobs target the LOCAL store, create-or-append by name: the wizard's
        // picker lists local playlists, and the GUI flow must never write to the
        // user's YouTube Music account.
        for job in &jobs {
            assert!(matches!(
                job.dest,
                TransferDest::LocalPlaylist { ref name } if name.as_deref() == Some("Mix")
            ));
        }
        assert_eq!(jobs[0].min_score, 0.80);
        assert_eq!(jobs[0].match_policy, MatchPolicy::Balanced);
        assert_eq!(jobs[0].auto_accept_ambiguous_min_score, Some(0.75));
    }

    #[test]
    fn existing_dest_resolves_the_local_playlist_and_rejects_unknown_ids() {
        let mut engine = engine();
        let playlist_id = engine.test_add_playlist("City Pop");

        let jobs = parse_jobs(
            &engine,
            serde_json::json!({
                "source_ids": ["one"],
                "dest": { "kind": "existing", "playlist_id": playlist_id }
            }),
        )
        .unwrap();
        assert!(matches!(
            jobs[0].dest,
            TransferDest::LocalPlaylist { ref name } if name.as_deref() == Some("City Pop")
        ));

        let unknown = parse_jobs(
            &engine,
            serde_json::json!({
                "source_ids": ["one"],
                "dest": { "kind": "existing", "playlist_id": "nope" }
            }),
        );
        assert_eq!(unknown.unwrap_err().0, "unknown_playlist");
    }

    #[test]
    fn invalid_specs_and_failure_paths_are_stable() {
        assert!(
            parse_jobs(
                &engine(),
                serde_json::json!({
                    "source_ids": [],
                    "dest": { "kind": "new", "name": "Mix" }
                }),
            )
            .is_err()
        );
        let mut state = TransferState::default();
        state.running(2, "Mix".to_owned());
        state.failed("network".to_owned());
        assert_eq!(state.phase, TransferPhaseModel::Failed);
        assert_eq!(state.error.as_deref(), Some("network"));
        assert!(state.job.is_none());
    }

    #[tokio::test]
    async fn cancel_is_admitted_without_network_work_and_clears_queued_jobs() {
        let (raw_tx, _raw_rx) = tokio::sync::mpsc::channel(8);
        let mut host = TransferHost::spawn(DaemonEventSender::new(raw_tx));
        host.pending.push_back(gui_job(
            TransferSource::SpotifyPlaylist {
                id: "p2".to_owned(),
            },
            TransferDest::YtmExistingPlaylist {
                name: "Mix".to_owned(),
            },
        ));
        let engine = super::super::engine::test_engine();

        let (response, publish) = host
            .command(&engine, RemoteCommand::TransferCancel)
            .expect("transfer command");
        assert!(response.ok);
        assert!(!publish);
        assert!(host.pending.is_empty());
    }

    #[test]
    fn auth_events_flip_engine_spotify_user_and_accounts_revision() {
        let mut engine = super::super::engine::test_engine();
        let rev = engine.accounts_rev();

        assert!(apply_account_event(
            &TransferEvent::AuthDone {
                display_name: "listener".to_owned(),
            },
            &mut engine,
        ));
        assert!(engine.accounts_rev() > rev);
        assert_eq!(
            engine.accounts_models().spotify.user.as_deref(),
            Some("listener")
        );
        let connected_rev = engine.accounts_rev();
        assert!(apply_account_event(
            &TransferEvent::Disconnected,
            &mut engine
        ));
        assert!(engine.accounts_rev() > connected_rev);
        assert_eq!(engine.accounts_models().spotify.user, None);
    }
}
