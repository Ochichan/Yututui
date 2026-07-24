//! Blocking portable-export worker for the standalone TUI runtime.

use std::path::PathBuf;

use super::{RuntimeEvent, RuntimeSender, emit};
use crate::app::{Msg, PersonalDataExportSources};

pub(super) fn spawn(
    tx: RuntimeSender,
    directory: PathBuf,
    schema: u32,
    sources: Box<PersonalDataExportSources>,
    reply: Option<crate::remote::RemoteReply>,
) {
    tokio::spawn(async move {
        let result = tokio::task::spawn_blocking(move || {
            let result = if schema == 1 {
                let snapshot = crate::data_export::ExportSnapshot::new(
                    &sources.config,
                    &sources.library,
                    &sources.playlists,
                    &sources.signals,
                    &sources.station,
                );
                // The portable projection is fully owned; release the live clone before
                // pretty-printing so peak memory does not include both representations.
                drop(sources);
                crate::data_export::export_snapshot(&directory, &snapshot)
            } else {
                crate::data_export::export_v2_from_sources(
                    &directory,
                    &sources.personal_state,
                    &sources.library,
                    &sources.playlists,
                    &sources.signals,
                    &sources.station,
                )
            };
            result
                .map_err(|error| crate::util::sanitize::sanitize_error_text(error.to_string()))
        })
        .await
        .unwrap_or_else(|error| Err(format!("personal-data export worker failed: {error}")));
        emit(
            &tx,
            RuntimeEvent::App(Msg::Data(crate::app::DataMsg::PersonalDataExport(
                crate::app::PersonalDataExportMsg::Finished { result, reply },
            ))),
        );
    });
}
