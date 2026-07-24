use std::time::Duration;

use anyhow::{Context, Result};

use crate::app::App;
use crate::persist;

/// Persist owner snapshots at every return boundary, including failures before actors start.
pub(super) async fn flush_owner_persistence(
    app: &App,
    persist: &persist::PersistHandle,
) -> Result<()> {
    if let Some(reason) = persist::persistence_access().read_only_reason() {
        tracing::info!(%reason, "read-only secondary has no persistence frontier to flush");
        return Ok(());
    }

    let personal_state = crate::personal_state::reconcile_runtime(
        &app.personal_state,
        &app.library,
        &app.playlists,
        &app.signals,
        &app.station,
    )
    .and_then(|state| {
        crate::personal_state::PersonalStateCommit::prepare_for_runtime(
            state,
            app.playlists.revision(),
        )
    })
    .map_err(anyhow::Error::msg)
    .context("could not prepare the final personal-state transaction")?;

    // Always publish every authoritative store. A store that happened not to receive a
    // runtime mutation is still part of the quit transaction; omitting it makes a flush timeout
    // silently depend on whichever commands happened to run during this session.
    let snapshots = [
        persist::Snapshot::Session(app.session_cache_snapshot()),
        persist::Snapshot::PersonalState(Box::new(personal_state)),
        persist::Snapshot::Downloads(app.download_store.clone()),
        persist::Snapshot::Config(Box::new(app.config.clone())),
        persist::Snapshot::RomanizedTitles(app.romanization.cache.clone()),
    ];
    persist
        .seal_with_snapshots(snapshots)
        .context("owner persistence seal was rejected")?;
    if !persist.flush(Duration::from_secs(5)).await {
        tracing::warn!(
            "persist flush failed or timed out at quit; retrying every shadow-owned frontier"
        );
        match tokio::time::timeout(Duration::from_secs(5), persist.fallback_newest_owned()).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                for (store, failure) in error.failures() {
                    tracing::warn!(
                        store = store.label(),
                        error = failure,
                        "quit fallback did not confirm durability; ownership retained"
                    );
                }
                return Err(error).context("owner persistence fallback failed");
            }
            Err(_) => {
                anyhow::bail!(
                    "owner persistence fallback timed out; newest operations remain recovery-owned"
                );
            }
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "runner/ordered_fallback_tests.rs"]
mod ordered_fallback_tests;
