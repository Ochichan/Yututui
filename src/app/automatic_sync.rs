//! TUI owner-lane adapter for the shared automatic personal-sync scheduler.

use super::*;
use crate::sync::service::SyncServiceError;

impl App {
    pub(super) fn finish_personal_sync_schedule(
        &mut self,
        reply: &PersonalSyncReply,
        error: SyncServiceError,
    ) -> Vec<Cmd> {
        let Some(token) = reply.token() else {
            return Vec::new();
        };
        let outcome = match error {
            SyncServiceError::Offline => {
                crate::sync::SyncAttemptOutcome::Offline { retry_after: None }
            }
            SyncServiceError::RateLimited(retry_after) => {
                crate::sync::SyncAttemptOutcome::Offline { retry_after }
            }
            _ => crate::sync::SyncAttemptOutcome::Failed,
        };
        self.finish_personal_sync_token(token, outcome)
    }

    pub(super) fn finish_personal_sync_schedule_success(
        &mut self,
        reply: &PersonalSyncReply,
    ) -> Vec<Cmd> {
        let Some(token) = reply.token() else {
            return Vec::new();
        };
        self.finish_personal_sync_token(token, crate::sync::SyncAttemptOutcome::Succeeded)
    }

    fn finish_personal_sync_token(
        &mut self,
        token: crate::sync::SyncAttemptToken,
        outcome: crate::sync::SyncAttemptOutcome,
    ) -> Vec<Cmd> {
        let mut commands = match self.personal_state.sync.scheduler.finish(
            token,
            outcome,
            std::time::Instant::now(),
        ) {
            crate::sync::SyncFinish::Accepted { next: Some(next) } => {
                self.start_automatic_sync(next)
            }
            crate::sync::SyncFinish::Accepted { next: None } | crate::sync::SyncFinish::Stale => {
                Vec::new()
            }
        };
        commands.extend(self.refresh_open_sync_ui());
        commands
    }

    fn start_automatic_sync(&mut self, token: crate::sync::SyncAttemptToken) -> Vec<Cmd> {
        if self.personal_sync_lane_busy() {
            match self.personal_state.sync.deferred_automatic {
                Some(deferred) => debug_assert_eq!(deferred, token),
                None => self.personal_state.sync.deferred_automatic = Some(token),
            }
            return Vec::new();
        }
        let mut commands = self.start_personal_sync_with_reply(
            PersonalSyncAction::AutomaticSync,
            PersonalSyncReply::automatic(token),
        );
        commands.extend(self.refresh_open_sync_ui());
        commands
    }

    fn personal_sync_lane_busy(&self) -> bool {
        self.personal_state.sync.in_progress
            || self.personal_state.sync_ui.remote_mutation_in_progress()
    }

    pub(super) fn resume_automatic_sync_if_ready(&mut self) -> Vec<Cmd> {
        if self.personal_sync_lane_busy() {
            return Vec::new();
        }
        if let Some(token) = self.personal_state.sync.deferred_automatic.take() {
            return self.start_automatic_sync(token);
        }
        self.poll_automatic_sync()
    }

    pub(crate) fn enable_automatic_sync(&mut self) -> Vec<Cmd> {
        if let Some(device_id) = self.personal_state.device_id.as_ref() {
            self.personal_state
                .sync
                .scheduler
                .configure_jitter(crate::sync::BackoffJitter::stable_for(device_id.as_str()));
        }
        let token = self
            .personal_state
            .sync
            .scheduler
            .enable(std::time::Instant::now());
        token.map_or_else(Vec::new, |token| self.start_automatic_sync(token))
    }

    pub(crate) fn disable_automatic_sync(&mut self) {
        self.personal_state.sync.scheduler.disable();
        if let Some(token) = self.personal_state.sync.deferred_automatic.take() {
            let _ = self.personal_state.sync.scheduler.finish(
                token,
                crate::sync::SyncAttemptOutcome::Failed,
                std::time::Instant::now(),
            );
        }
    }

    pub(crate) fn automatic_sync_deadline(&self) -> Option<std::time::Instant> {
        if self.personal_sync_lane_busy() {
            return None;
        }
        self.personal_state.sync.scheduler.next_deadline()
    }

    pub(crate) fn automatic_sync_waiting_for_connectivity(&self) -> bool {
        !self.personal_sync_lane_busy()
            && self
                .personal_state
                .sync
                .scheduler
                .waiting_for_connectivity()
    }

    pub(crate) fn poll_automatic_sync(&mut self) -> Vec<Cmd> {
        if self.personal_sync_lane_busy() {
            return Vec::new();
        }
        let token = self
            .personal_state
            .sync
            .scheduler
            .poll(std::time::Instant::now());
        token.map_or_else(Vec::new, |token| self.start_automatic_sync(token))
    }

    pub(crate) fn note_personal_state_mutation(&mut self) -> Vec<Cmd> {
        let token = self
            .personal_state
            .sync
            .scheduler
            .local_mutation(std::time::Instant::now());
        token.map_or_else(Vec::new, |token| self.start_automatic_sync(token))
    }

    pub(crate) fn note_webdav_connectivity(&mut self) -> Vec<Cmd> {
        let token = self
            .personal_state
            .sync
            .scheduler
            .network_reconnected(std::time::Instant::now());
        token.map_or_else(Vec::new, |token| self.start_automatic_sync(token))
    }

    fn refresh_open_sync_ui(&mut self) -> Vec<Cmd> {
        if self.mode != Mode::Settings
            || !self
                .settings
                .as_ref()
                .is_some_and(|settings| settings.tab == SettingsTab::Sync)
            || self.personal_state.sync_ui.flow_id == 0
        {
            return Vec::new();
        }
        self.queue_sync_ui_refresh();
        self.start_pending_sync_ui_refresh()
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::*;
    use crate::app::sync_ui::SyncBusy;
    use tokio::sync::oneshot;

    fn configured_app(device_id: &str) -> App {
        let mut app = App::new(50);
        app.personal_state.device_id =
            Some(crate::personal_state::DeviceId::new(device_id).unwrap());
        app
    }

    #[test]
    fn activation_race_defers_the_exact_automatic_token() {
        let mut app = configured_app("device-a");
        app.personal_state.sync.in_progress = true;

        assert!(app.enable_automatic_sync().is_empty());
        let token = app
            .personal_state
            .sync
            .scheduler
            .in_flight()
            .expect("startup token remains owned");
        assert_eq!(
            token.kind(),
            crate::sync::SyncAttemptKind::Automatic(crate::sync::AutomaticSyncTrigger::Startup)
        );
        assert_eq!(app.personal_state.sync.deferred_automatic, Some(token));
        assert_eq!(app.automatic_sync_deadline(), None);

        app.personal_state.sync.in_progress = false;
        assert!(matches!(
            app.resume_automatic_sync_if_ready().as_slice(),
            [Cmd::Data(DataCmd::PersonalSync {
                action: PersonalSyncAction::AutomaticSync,
                ..
            })]
        ));
        assert_eq!(app.personal_state.sync.scheduler.in_flight(), Some(token));
        assert_eq!(app.personal_state.sync.deferred_automatic, None);
    }

    #[test]
    fn pairing_mutation_defers_automatic_sync_until_the_owner_lane_settles() {
        let mut app = configured_app("device-a");
        app.personal_state.sync_ui.busy = Some(SyncBusy::PairHostApprove);

        assert!(app.enable_automatic_sync().is_empty());
        let token = app
            .personal_state
            .sync
            .scheduler
            .in_flight()
            .expect("startup token remains owned");
        assert_eq!(app.personal_state.sync.deferred_automatic, Some(token));

        app.personal_state.sync_ui.busy = None;
        assert!(matches!(
            app.start_pending_sync_ui_refresh().as_slice(),
            [Cmd::Data(DataCmd::PersonalSync {
                action: PersonalSyncAction::AutomaticSync,
                ..
            })]
        ));
        assert_eq!(app.personal_state.sync.scheduler.in_flight(), Some(token));
        assert_eq!(app.personal_state.sync.deferred_automatic, None);
    }

    #[test]
    fn due_automatic_cause_is_not_consumed_while_pairing_mutates_the_vault() {
        let mut app = configured_app("device-a");
        let due_at = Instant::now()
            .checked_sub(crate::sync::FALLBACK_INTERVAL)
            .expect("test clock has a fallback interval");
        let startup = app.personal_state.sync.scheduler.enable(due_at).unwrap();
        let _ = app.personal_state.sync.scheduler.finish(
            startup,
            crate::sync::SyncAttemptOutcome::Succeeded,
            due_at,
        );
        app.personal_state.sync_ui.busy = Some(SyncBusy::PairHostApprove);

        assert!(app.poll_automatic_sync().is_empty());
        assert_eq!(app.personal_state.sync.scheduler.in_flight(), None);

        app.personal_state.sync_ui.busy = None;
        assert!(matches!(
            app.resume_automatic_sync_if_ready().as_slice(),
            [Cmd::Data(DataCmd::PersonalSync {
                action: PersonalSyncAction::AutomaticSync,
                ..
            })]
        ));
        assert!(matches!(
            app.personal_state
                .sync
                .scheduler
                .in_flight()
                .unwrap()
                .kind(),
            crate::sync::SyncAttemptKind::Automatic(crate::sync::AutomaticSyncTrigger::Fallback)
        ));
    }

    #[test]
    fn refresh_and_recovery_work_do_not_block_personal_sync() {
        for busy in [SyncBusy::Refresh, SyncBusy::RecoveryExport] {
            let mut app = configured_app("device-a");
            app.personal_state.sync_ui.busy = Some(busy);

            assert!(matches!(
                app.enable_automatic_sync().as_slice(),
                [Cmd::Data(DataCmd::PersonalSync {
                    action: PersonalSyncAction::AutomaticSync,
                    ..
                })]
            ));
        }
    }

    #[test]
    fn remote_sync_now_is_rejected_while_pairing_mutates_the_vault() {
        let mut app = configured_app("device-a");
        app.personal_state.sync_ui.busy = Some(SyncBusy::PairHostApprove);
        let (reply, mut response) = oneshot::channel();

        let commands = app.start_personal_sync(PersonalSyncAction::SyncNow, reply.into());

        assert!(commands.is_empty());
        assert_eq!(
            response.try_recv().unwrap().reason.as_deref(),
            Some("sync_busy")
        );
        assert!(!app.personal_state.sync.in_progress);
        assert_eq!(app.personal_state.sync.scheduler.in_flight(), None);
    }

    #[test]
    fn successful_webdav_pairing_poll_bypasses_only_latched_offline_backoff() {
        let mut app = configured_app("device-a");
        let _ = app.enable_automatic_sync();
        let startup = app.personal_state.sync.scheduler.in_flight().unwrap();
        let retry_due_before = Instant::now();
        let _ = app.personal_state.sync.scheduler.finish(
            startup,
            crate::sync::SyncAttemptOutcome::Offline { retry_after: None },
            retry_due_before,
        );
        app.personal_state.sync.in_progress = false;
        app.personal_state.sync.pending_reply = None;
        app.personal_state.sync_ui.flow_id = 1;
        app.personal_state.sync_ui.busy = Some(SyncBusy::PairJoinPoll);

        let commands = app.reduce_sync_ui_event(SyncUiEvent::JoinPolled {
            flow_id: 1,
            result: Box::new(Ok(None)),
        });
        assert!(commands.iter().any(|command| matches!(
            command,
            Cmd::Data(DataCmd::PersonalSync {
                action: PersonalSyncAction::AutomaticSync,
                ..
            })
        )));
        assert!(matches!(
            app.personal_state
                .sync
                .scheduler
                .in_flight()
                .unwrap()
                .kind(),
            crate::sync::SyncAttemptKind::Automatic(
                crate::sync::AutomaticSyncTrigger::NetworkReconnect
            )
        ));
    }

    #[test]
    fn successful_webdav_work_without_offline_latch_does_not_add_an_attempt() {
        let mut app = configured_app("device-a");
        let _ = app.enable_automatic_sync();
        let startup = app.personal_state.sync.scheduler.in_flight().unwrap();
        let now = Instant::now();
        let _ = app.personal_state.sync.scheduler.finish(
            startup,
            crate::sync::SyncAttemptOutcome::Succeeded,
            now,
        );
        app.personal_state.sync.in_progress = false;
        app.personal_state.sync.pending_reply = None;
        let fallback_due = app.personal_state.sync.scheduler.next_deadline();
        app.personal_state.sync_ui.flow_id = 1;
        app.personal_state.sync_ui.busy = Some(SyncBusy::PairJoinPoll);

        assert!(
            app.reduce_sync_ui_event(SyncUiEvent::JoinPolled {
                flow_id: 1,
                result: Box::new(Ok(None)),
            })
            .is_empty()
        );
        assert_eq!(app.personal_state.sync.scheduler.in_flight(), None);
        assert_eq!(
            app.personal_state.sync.scheduler.next_deadline(),
            fallback_due
        );
    }

    #[test]
    fn an_open_sync_page_refreshes_for_automatic_start_and_settlement() {
        let mut app = configured_app("device-a");
        app.open_settings();
        app.settings.as_mut().unwrap().tab = SettingsTab::Sync;
        app.personal_state.sync_ui.flow_id = 7;

        let started = app.enable_automatic_sync();
        assert!(started.iter().any(|command| matches!(
            command,
            Cmd::Data(DataCmd::PersonalSync {
                action: PersonalSyncAction::AutomaticSync,
                ..
            })
        )));
        assert!(started.iter().any(|command| matches!(
            command,
            Cmd::Data(DataCmd::SyncUi(SyncUiCommand::Refresh {
                flow_id: 7,
                in_progress: true,
                ..
            }))
        )));

        let reply = app
            .personal_state
            .sync
            .pending_reply
            .clone()
            .expect("automatic reply");
        app.personal_state.sync.in_progress = false;
        app.personal_state.sync.pending_reply = None;
        app.personal_state.sync_ui.busy = None;
        app.personal_state.sync_ui.refresh_in_flight = None;
        let settled = app.finish_personal_sync_schedule(&reply, SyncServiceError::Offline);
        assert!(settled.iter().any(|command| matches!(
            command,
            Cmd::Data(DataCmd::SyncUi(SyncUiCommand::Refresh {
                flow_id: 7,
                in_progress: false,
                ..
            }))
        )));
    }

    #[test]
    fn a_previously_visited_sync_page_does_not_refresh_while_closed() {
        let mut app = configured_app("device-a");
        app.personal_state.sync_ui.flow_id = 7;

        let commands = app.enable_automatic_sync();

        assert!(commands.iter().any(|command| matches!(
            command,
            Cmd::Data(DataCmd::PersonalSync {
                action: PersonalSyncAction::AutomaticSync,
                ..
            })
        )));
        assert!(!commands.iter().any(|command| matches!(
            command,
            Cmd::Data(DataCmd::SyncUi(SyncUiCommand::Refresh { .. }))
        )));
    }

    #[test]
    fn debounce_starts_only_for_the_latest_durable_personal_state_revision() {
        let mut app = configured_app("device-a");
        app.personal_state.ledger.revision = 7;
        let now = Instant::now();
        let startup = app.personal_state.sync.scheduler.enable(now).unwrap();
        let _ = app.personal_state.sync.scheduler.finish(
            startup,
            crate::sync::SyncAttemptOutcome::Succeeded,
            now,
        );
        let fallback = app.personal_state.sync.scheduler.next_deadline();
        let state_identity = app.personal_state.ledger.identity().unwrap();

        assert!(
            app.update(Msg::PersonalStatePersisted {
                revision: 6,
                state_identity: state_identity.clone(),
            })
            .is_empty()
        );
        assert_eq!(app.personal_state.sync.scheduler.next_deadline(), fallback);
        assert!(
            app.update(Msg::PersonalStatePersisted {
                revision: 7,
                state_identity: "stale-state-identity".to_owned(),
            })
            .is_empty()
        );
        assert_eq!(app.personal_state.sync.scheduler.next_deadline(), fallback);

        let before = Instant::now();
        assert!(
            app.update(Msg::PersonalStatePersisted {
                revision: 7,
                state_identity,
            })
            .is_empty()
        );
        let debounce = app
            .personal_state
            .sync
            .scheduler
            .next_deadline()
            .unwrap()
            .duration_since(before);
        assert!(debounce >= crate::sync::LOCAL_MUTATION_DEBOUNCE);
        assert!(debounce <= crate::sync::LOCAL_MUTATION_DEBOUNCE + Duration::from_secs(1));
    }

    #[test]
    fn owner_configures_stable_device_jitter_before_enabling() {
        let mut app = configured_app("device-a");
        assert_eq!(app.enable_automatic_sync().len(), 1);
        let token = app
            .personal_state
            .sync
            .scheduler
            .in_flight()
            .expect("startup token");
        let now = Instant::now();
        let _ = app.personal_state.sync.scheduler.finish(
            token,
            crate::sync::SyncAttemptOutcome::Offline { retry_after: None },
            now,
        );

        let mut expected = crate::sync::AutomaticSyncScheduler::new(
            crate::sync::BackoffJitter::stable_for("device-a"),
        );
        let expected_token = expected.enable(now).unwrap();
        let _ = expected.finish(
            expected_token,
            crate::sync::SyncAttemptOutcome::Offline { retry_after: None },
            now,
        );
        let actual_delay = app
            .personal_state
            .sync
            .scheduler
            .next_deadline()
            .unwrap()
            .duration_since(now);
        assert_eq!(
            actual_delay,
            expected.next_deadline().unwrap().duration_since(now)
        );
        assert!(actual_delay >= crate::sync::BACKOFF_MIN);
    }

    #[test]
    fn youtube_api_success_does_not_bypass_webdav_backoff() {
        let mut app = configured_app("device-a");
        let _ = app.enable_automatic_sync();
        let token = app
            .personal_state
            .sync
            .scheduler
            .in_flight()
            .expect("startup token");
        let now = Instant::now();
        let _ = app.personal_state.sync.scheduler.finish(
            token,
            crate::sync::SyncAttemptOutcome::Offline { retry_after: None },
            now,
        );
        app.personal_state.sync.in_progress = false;
        app.personal_state.sync.pending_reply = None;
        let retry_due = app.personal_state.sync.scheduler.next_deadline();

        let _ = app.update(Msg::ApiModeResolved {
            mode: ApiMode::Authenticated,
            had_cookie: false,
        });

        assert_eq!(app.personal_state.sync.scheduler.in_flight(), None);
        assert_eq!(app.personal_state.sync.scheduler.next_deadline(), retry_due);
    }

    #[test]
    fn rate_limit_retry_hint_is_forwarded_to_the_scheduler() {
        let mut app = configured_app("device-a");
        let _ = app.enable_automatic_sync();
        let token = app
            .personal_state
            .sync
            .scheduler
            .in_flight()
            .expect("startup token");
        let reply = PersonalSyncReply::automatic(token);
        let before = Instant::now();

        assert!(
            app.finish_personal_sync_schedule(
                &reply,
                SyncServiceError::RateLimited(Some(Duration::from_secs(75))),
            )
            .is_empty()
        );

        let delay = app
            .personal_state
            .sync
            .scheduler
            .next_deadline()
            .unwrap()
            .duration_since(before);
        assert!(delay >= Duration::from_secs(75));
        assert!(delay <= Duration::from_secs(76));
    }
}
