use super::*;

/// Direct-URL retry bookkeeping for watch-URL loads (mirrors the TUI's
/// `YtdlpHeal::load_retry_attempted`).
#[derive(Default)]
pub(super) struct LoadRetry {
    /// Track whose resolve is in flight. Every load clears it, so a late result cannot restart
    /// a track the user already replaced or replayed.
    pub(super) pending: Option<String>,
    /// Tracks that already used their retry; a track leaves the set once it plays.
    pub(super) attempted: HashSet<String>,
    /// Resolved direct URL that replaces the watch URL for the next load of this track only.
    pub(super) direct: Option<(String, String)>,
    /// A resolve with the session's cookies has been sent. yt-dlp reports rejected cookies only
    /// while resolving, so without a load retry the first YouTube track that plays sends one
    /// resolve; with no retry pending, its result yields only the cookie verdict.
    pub(super) cookies_checked: bool,
}

impl DaemonEngine {
    /// Mirrors the TUI's load retry: a watch-URL load that mpv reports only as `loading failed`
    /// is usually its ytdl_hook getting no playable format from one YouTube response, which a
    /// fresh resolver extraction often does not repeat. Midtrack failures belong to source
    /// recovery, and the update heal's in-flight latch excludes this retry.
    pub(super) fn begin_load_retry(&mut self, error: &str) -> Option<EngineEffect> {
        let initial_load = self
            .playback
            .time_pos
            .is_none_or(|position| !position.is_finite() || position <= 0.0);
        if !initial_load
            || crate::player::recovery::classify_source_failure(error)
                != Some(crate::player::recovery::RecoverableSourceFailure::GenericLoadingFailure)
            || self.heal_pending.is_some()
            || self.load_retry.pending.is_some()
        {
            return None;
        }
        let song = self.queue.current()?;
        if song.is_radio_station() || self.load_retry.attempted.contains(&song.video_id) {
            return None;
        }
        let watch_url = song.prefetch_target()?;
        let video_id = song.video_id.clone();
        if self.load_retry.attempted.len() >= crate::playback_policy::HEAL_ATTEMPTED_MAX {
            self.load_retry.attempted.clear();
        }
        self.load_retry.attempted.insert(video_id.clone());
        self.load_retry.pending = Some(video_id.clone());
        self.load_retry.cookies_checked = true;
        Some(EngineEffect::ResolveRetry {
            video_id,
            watch_url,
            cookies: self
                .config
                .cookies_file_for_external_tools(data_dir().as_deref()),
        })
    }

    /// Send the session's one cookie-check resolve once a YouTube track plays. With no retry
    /// pending for the track, [`Self::handle_resolve_retry`] uses its result only to report
    /// rejected cookies.
    pub(super) fn begin_cookie_check(&mut self) -> Option<EngineEffect> {
        if self.load_retry.cookies_checked {
            return None;
        }
        let song = self.queue.current()?;
        if !song.resolves_through_youtube() {
            return None;
        }
        let watch_url = song.prefetch_target()?;
        let video_id = song.video_id.clone();
        self.load_retry.cookies_checked = true;
        let cookies = self
            .config
            .cookies_file_for_external_tools(data_dir().as_deref())?;
        Some(EngineEffect::ResolveRetry {
            video_id,
            watch_url,
            cookies: Some(cookies),
        })
    }

    /// Finish a load retry: reload the same track from the resolved direct URL, or take the
    /// plain error path when resolution failed. Rejected cookies are reported either way,
    /// because yt-dlp often still resolves through anonymous clients.
    pub async fn handle_resolve_retry(
        &mut self,
        video_id: String,
        stream_url: Option<String>,
        cookies_rejected: bool,
    ) -> Vec<EngineEffect> {
        let (effects, load_failed) = self.finish_resolve_retry(video_id, stream_url).await;
        if cookies_rejected && !load_failed {
            tracing::warn!("yt-dlp reports the YouTube cookies are no longer valid");
            self.last_error = Some(crate::resolver::REJECTED_COOKIES_NOTICE.to_owned());
        }
        effects
    }

    async fn finish_resolve_retry(
        &mut self,
        video_id: String,
        stream_url: Option<String>,
    ) -> (Vec<EngineEffect>, bool) {
        if self.load_retry.pending.as_deref() != Some(video_id.as_str()) {
            return (Vec::new(), false); // stale: an ordinary load superseded the retry
        }
        self.load_retry.pending = None;
        if self.queue.current().is_none_or(|s| s.video_id != video_id) {
            return (Vec::new(), false);
        }
        let Some(stream_url) = stream_url else {
            let effects = self
                .handle_playback_error("mpv could not load this stream".to_owned())
                .await;
            return (effects, false);
        };
        self.load_retry.direct = Some((video_id, stream_url));
        let loaded = self.load_current().await;
        self.load_retry.direct = None;
        if let Err(e) = loaded {
            self.last_error = Some(e.to_string());
            self.stop_playback();
            return (Vec::new(), true);
        }
        (Vec::new(), false)
    }

    /// Every load supersedes an in-flight load retry or update heal, so a late result cannot
    /// restart a track the user already replayed or replaced. The resolved direct URL replaces
    /// the watch URL only for the retried track's load.
    pub(super) fn load_retry_target(
        &mut self,
        video_id: &str,
        target: crate::playback_target::PlaybackDestination,
    ) -> crate::playback_target::PlaybackDestination {
        self.load_retry.pending = None;
        self.heal_pending = None;
        match self.load_retry.direct.take() {
            Some((retry_id, url)) if retry_id == video_id => {
                crate::playback_target::PlaybackDestination::direct(url)
            }
            _ => target,
        }
    }
}
