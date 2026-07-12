//! Blocking portable-export worker for the standalone TUI runtime.

use std::path::PathBuf;

use super::{RuntimeEvent, RuntimeSender, emit};
use crate::app::{Msg, PersonalDataExportSources};

pub(super) fn spawn(
    tx: RuntimeSender,
    directory: PathBuf,
    sources: Box<PersonalDataExportSources>,
    reply: Option<crate::remote::RemoteReply>,
) {
    tokio::spawn(async move {
        let result = tokio::task::spawn_blocking(move || {
            let snapshot = crate::data_export::ExportSnapshot::new(
                &sources.config,
                &sources.library,
                &sources.playlists,
                &sources.signals,
                &sources.station,
            );
            // The portable projection is fully owned; release the live clone before pretty-printing
            // so peak memory does not include both complete representations.
            drop(sources);
            crate::data_export::export_snapshot(&directory, &snapshot)
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
