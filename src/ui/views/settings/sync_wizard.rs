//! Top-level modal rendering for personal-sync setup and device lifecycle.
//!
//! Move-only connection and pairing values stay in `App`. These renderers borrow them only for
//! the current frame and publish semantic mouse targets that contain no secret data.

mod form;
mod lifecycle;
mod support;

use ratatui::Frame;
use ratatui::layout::Rect;

use crate::app::{App, SyncWizard};

pub(crate) fn render_sync_wizard(frame: &mut Frame, app: &App, area: Rect) {
    let Some(wizard) = app.personal_state.sync_ui.wizard.as_ref() else {
        return;
    };
    match wizard {
        SyncWizard::Setup { form, confirm } => {
            form::render_connection(frame, app, area, form, false, *confirm)
        }
        SyncWizard::Join { form, confirm } => {
            form::render_connection(frame, app, area, form, true, *confirm)
        }
        SyncWizard::Host {
            code,
            expires_at_unix,
            host: _,
            review,
        } => lifecycle::render_host(
            frame,
            app,
            area,
            code.as_str(),
            *expires_at_unix,
            review.as_deref(),
        ),
        SyncWizard::JoinWaiting(_) => lifecycle::render_join_waiting(frame, app, area),
        SyncWizard::JoinPreview(preview) => {
            lifecycle::render_join_preview(frame, app, area, &preview.summary)
        }
        SyncWizard::DiscardJoinConfirm => lifecycle::render_discard_join(frame, app, area),
        SyncWizard::Revoke {
            device_id,
            device_name,
        } => lifecycle::render_revoke(frame, app, area, device_id, device_name),
        SyncWizard::Recovery(form) => form::render_recovery(frame, app, area, form),
        SyncWizard::Result { success, message } => {
            lifecycle::render_result(frame, app, area, *success, message)
        }
    }
}
