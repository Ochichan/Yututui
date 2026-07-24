//! Bounded capture of daemon-owned stores for the off-loop personal-data exporter.

use super::DaemonEngine;
use crate::config::Config;
use crate::library::Library;
use crate::personal_state::PersonalStateV2;
use crate::signals::Signals;
use crate::station::StationStore;

impl DaemonEngine {
    /// Fold mutable playback preferences into the config, validate every dynamic allocation, then
    /// clone. Playlists are immutable to the daemon and are loaded directly by the worker.
    pub(crate) fn personal_export_sources(
        &self,
    ) -> Result<
        (
            PersonalStateV2,
            Config,
            Library,
            Signals,
            StationStore,
            usize,
        ),
        String,
    > {
        let mut config = self.config.clone();
        config.volume = self.playback.volume;
        config.speed = Some(self.playback.speed);
        config.shuffle = Some(self.queue.shuffle);
        config.repeat = self.queue.repeat;
        config.autoplay_streaming = Some(self.streaming);
        let estimated_bytes = crate::data_export::live::validate_source_clone(
            &config,
            &self.library,
            None,
            &self.signals,
            &self.station,
        )
        .map_err(|error| {
            format!(
                "personal-data export is too large or complex to copy safely while ytt is running: {}. Stop the daemon, then run `ytt data export`, or reduce the saved metadata.",
                error.detail()
            )
        })?;
        Ok((
            self.personal_state.clone(),
            config,
            self.library.clone(),
            self.signals.clone(),
            self.station.clone(),
            estimated_bytes,
        ))
    }
}
