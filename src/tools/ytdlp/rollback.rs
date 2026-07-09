use super::*;

pub(super) async fn to_previous(cfg: &ToolsConfig, reason: &str) -> UpdateOutcome {
    use UpdateOutcome::Unavailable;

    if !cfg.managed_enabled() {
        return Unavailable("managed yt-dlp is disabled".into());
    }
    if !matches!(
        crate::tools::ytdlp_selection().map(|selection| selection.source),
        Some(crate::tools::YtdlpSource::Managed)
    ) {
        return Unavailable("managed yt-dlp is not the active selection".into());
    }
    let (Some(dir), Some(dest), Some(previous)) =
        (tools_dir(), managed_path(), previous_managed_path())
    else {
        return Unavailable("no data directory on this platform".into());
    };
    if !previous.is_file() {
        return Unavailable("no previous managed yt-dlp binary retained".into());
    }
    let Some(_lock) = acquire_update_lock_observing(&dir).await else {
        crate::tools::refresh_selection(cfg).await;
        return Unavailable("another update is already running".into());
    };

    let mut state = load_state();
    if let Some(expected) = state.previous_sha256.as_deref()
        && previous_stamp_matches(&previous, &state)
    {
        match sha256_file(&previous) {
            Ok(actual) if expected.eq_ignore_ascii_case(&actual) => {}
            Ok(_) => return Unavailable("previous yt-dlp checksum mismatch".into()),
            Err(error) => {
                return Unavailable(format!(
                    "cannot hash previous yt-dlp {}: {error}",
                    previous.display()
                ));
            }
        }
    }

    let installed = match inspect_binary(&previous).await {
        Ok(inspection) => inspection,
        Err(error) => {
            return Unavailable(format!("previous managed yt-dlp is not usable: {error}"));
        }
    };
    if let Some(expected) = state.previous_version.as_deref()
        && installed.version != expected
    {
        return Unavailable(format!(
            "previous yt-dlp reports {}, expected {expected}",
            installed.version
        ));
    }

    let tmp = match copy_binary_to_install_temp(&previous, &dir) {
        Ok(tmp) => tmp,
        Err(error) => {
            return Unavailable(format!(
                "failed to stage previous yt-dlp {}: {error}",
                previous.display()
            ));
        }
    };
    if let Err(error) = install_file(&tmp, &dest) {
        let _ = std::fs::remove_file(&tmp);
        return Unavailable(format!(
            "failed to rollback managed yt-dlp {}: {error}",
            dest.display()
        ));
    }

    let current = match inspect_binary(&dest).await {
        Ok(inspection) => inspection,
        Err(error) => {
            return Unavailable(format!(
                "rolled back managed yt-dlp failed verification: {error}"
            ));
        }
    };
    if current.sha256 != installed.sha256 || current.version != installed.version {
        return Unavailable("rolled back yt-dlp does not match previous binary".into());
    }

    let previous_channel = state.previous_channel;
    record_current_from_inspection(&mut state, previous_channel, &current);
    state.last_rollback_unix = Some(now_unix());
    remove_probe_cache_for(&dest, &mut state);
    save_state(&state);
    crate::tools::refresh_selection(cfg).await;
    tracing::warn!(
        reason,
        version = %current.version,
        "rolled back managed yt-dlp to previous binary"
    );
    UpdateOutcome::Installed {
        version: current.version,
    }
}
