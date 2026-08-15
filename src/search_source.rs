use serde::{Deserialize, Serialize};

/// Search/playback source selected from the Search screen and persisted in config.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum SearchSource {
    #[default]
    Youtube,
    SoundCloud,
    Audius,
    Jamendo,
    InternetArchive,
    OpenSubsonic,
    RadioBrowser,
    All,
}

impl SearchSource {
    pub const CONCRETE: [SearchSource; 7] = [
        SearchSource::Youtube,
        SearchSource::SoundCloud,
        SearchSource::Audius,
        SearchSource::Jamendo,
        SearchSource::InternetArchive,
        SearchSource::OpenSubsonic,
        SearchSource::RadioBrowser,
    ];

    pub fn code(self) -> &'static str {
        match self {
            SearchSource::Youtube => "YT",
            SearchSource::SoundCloud => "SC",
            SearchSource::Audius => "AU",
            SearchSource::Jamendo => "JA",
            SearchSource::InternetArchive => "IA",
            SearchSource::OpenSubsonic => "SUB",
            SearchSource::RadioBrowser => "RAD",
            SearchSource::All => "ALL",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            SearchSource::Youtube => "YouTube",
            SearchSource::SoundCloud => "SoundCloud",
            SearchSource::Audius => "Audius",
            SearchSource::Jamendo => "Jamendo",
            SearchSource::InternetArchive => "Internet Archive",
            SearchSource::OpenSubsonic => "Music server",
            SearchSource::RadioBrowser => "Radio Browser",
            SearchSource::All => "All enabled",
        }
    }

    pub fn id_prefix(self) -> &'static str {
        match self {
            SearchSource::Youtube => "yt",
            SearchSource::SoundCloud => "sc",
            SearchSource::Audius => "au",
            SearchSource::Jamendo => "ja",
            SearchSource::InternetArchive => "ia",
            SearchSource::OpenSubsonic => "sub",
            SearchSource::RadioBrowser => "rad",
            SearchSource::All => "all",
        }
    }

    pub fn is_youtube(source: &Self) -> bool {
        *source == SearchSource::Youtube
    }
}

/// Persisted search-source preferences and provider identifiers.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct SearchConfig {
    /// The source selected by default in the search box.
    pub source: SearchSource,
    /// The source used to fetch autoplay/DJ Gem streaming candidates.
    ///
    /// Radio Browser entries are live streams rather than tracks, so normalization keeps
    /// `RadioBrowser` out of this setting even if it is enabled for the Search screen.
    pub streaming_source: SearchSource,
    pub youtube: bool,
    pub soundcloud: bool,
    pub audius: bool,
    pub jamendo: bool,
    pub internet_archive: bool,
    /// Enabled only while a complete OpenSubsonic profile is active.
    pub open_subsonic: bool,
    pub radio_browser: bool,
    /// Audius requires an app identifier on public API calls. Not a secret.
    pub audius_app_name: Option<String>,
    /// Jamendo requires a client_id for its public API. Not treated as a secret.
    pub jamendo_client_id: Option<String>,
}

impl Default for SearchConfig {
    fn default() -> Self {
        Self {
            source: SearchSource::Youtube,
            streaming_source: SearchSource::Youtube,
            youtube: true,
            soundcloud: true,
            audius: true,
            jamendo: true,
            internet_archive: true,
            open_subsonic: false,
            radio_browser: true,
            audius_app_name: None,
            jamendo_client_id: None,
        }
    }
}

impl SearchConfig {
    pub fn is_enabled(&self, source: SearchSource) -> bool {
        match source {
            SearchSource::Youtube => self.youtube,
            SearchSource::SoundCloud => self.soundcloud,
            SearchSource::Audius => self.audius,
            SearchSource::Jamendo => self.jamendo,
            SearchSource::InternetArchive => self.internet_archive,
            SearchSource::OpenSubsonic => self.open_subsonic,
            SearchSource::RadioBrowser => self.radio_browser,
            SearchSource::All => true,
        }
    }

    pub fn set_enabled(&mut self, source: SearchSource, enabled: bool) {
        match source {
            SearchSource::Youtube => self.youtube = enabled,
            SearchSource::SoundCloud => self.soundcloud = enabled,
            SearchSource::Audius => self.audius = enabled,
            SearchSource::Jamendo => self.jamendo = enabled,
            SearchSource::InternetArchive => self.internet_archive = enabled,
            SearchSource::OpenSubsonic => self.open_subsonic = enabled,
            SearchSource::RadioBrowser => self.radio_browser = enabled,
            SearchSource::All => {}
        }
        self.source = self.normalized_source(self.source);
        self.streaming_source = self.normalized_streaming_source(self.streaming_source);
    }

    pub fn enabled_sources(&self) -> Vec<SearchSource> {
        SearchSource::CONCRETE
            .into_iter()
            .filter(|&source| self.is_enabled(source))
            .collect()
    }

    /// Sources handled by the existing public-provider search client.
    ///
    /// OpenSubsonic owns credentials and origin policy in a separate actor, so it must never
    /// reach the public-provider fan-out even when it participates in the app-level `All` view.
    pub fn enabled_public_sources(&self) -> Vec<SearchSource> {
        self.enabled_sources()
            .into_iter()
            .filter(|source| *source != SearchSource::OpenSubsonic)
            .collect()
    }

    pub fn selectable_sources(&self) -> Vec<SearchSource> {
        let mut sources = self.enabled_sources();
        if sources.len() > 1 {
            sources.push(SearchSource::All);
        }
        if sources.is_empty() {
            sources.push(SearchSource::Youtube);
        }
        sources
    }

    pub fn streaming_enabled_sources(&self) -> Vec<SearchSource> {
        self.enabled_sources()
            .into_iter()
            .filter(|source| {
                !matches!(
                    *source,
                    SearchSource::OpenSubsonic | SearchSource::RadioBrowser
                )
            })
            .collect()
    }

    pub fn selectable_streaming_sources(&self) -> Vec<SearchSource> {
        let mut sources = self.streaming_enabled_sources();
        if sources.len() > 1 {
            sources.push(SearchSource::All);
        }
        if sources.is_empty() {
            sources.push(SearchSource::Youtube);
        }
        sources
    }

    pub fn normalized_source(&self, source: SearchSource) -> SearchSource {
        if source == SearchSource::All {
            return if self.enabled_sources().len() > 1 {
                SearchSource::All
            } else {
                self.enabled_sources()
                    .into_iter()
                    .next()
                    .unwrap_or(SearchSource::Youtube)
            };
        }
        if self.is_enabled(source) {
            source
        } else {
            self.enabled_sources()
                .into_iter()
                .next()
                .unwrap_or(SearchSource::Youtube)
        }
    }

    pub fn normalized_streaming_source(&self, source: SearchSource) -> SearchSource {
        if source == SearchSource::All {
            return if self.streaming_enabled_sources().len() > 1 {
                SearchSource::All
            } else {
                self.streaming_enabled_sources()
                    .into_iter()
                    .next()
                    .unwrap_or(SearchSource::Youtube)
            };
        }
        if !matches!(
            source,
            SearchSource::OpenSubsonic | SearchSource::RadioBrowser
        ) && self.is_enabled(source)
        {
            source
        } else {
            self.streaming_enabled_sources()
                .into_iter()
                .next()
                .unwrap_or(SearchSource::Youtube)
        }
    }

    pub fn cycled_source(&self, current: SearchSource, forward: bool) -> SearchSource {
        let sources = self.selectable_sources();
        let i = sources.iter().position(|&s| s == current).unwrap_or(0);
        let n = sources.len();
        if n == 0 {
            return SearchSource::Youtube;
        }
        let j = if forward {
            (i + 1) % n
        } else {
            (i + n - 1) % n
        };
        sources[j]
    }

    pub fn cycled_streaming_source(&self, current: SearchSource, forward: bool) -> SearchSource {
        let sources = self.selectable_streaming_sources();
        let i = sources.iter().position(|&s| s == current).unwrap_or(0);
        let n = sources.len();
        if n == 0 {
            return SearchSource::Youtube;
        }
        let j = if forward {
            (i + 1) % n
        } else {
            (i + n - 1) % n
        };
        sources[j]
    }

    pub fn normalized(mut self) -> Self {
        self.source = self.normalized_source(self.source);
        self.streaming_source = self.normalized_streaming_source(self.streaming_source);
        self.audius_app_name = trim_to_option(self.audius_app_name.as_deref());
        self.jamendo_client_id = trim_to_option(self.jamendo_client_id.as_deref());
        self
    }

    pub fn effective_audius_app_name(&self) -> String {
        self.audius_app_name
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("yututui")
            .to_owned()
    }

    pub fn jamendo_client_id(&self) -> Option<&str> {
        self.jamendo_client_id
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
    }
}

fn trim_to_option(s: Option<&str>) -> Option<String> {
    s.map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn streaming_sources_exclude_live_radio_and_open_subsonic() {
        let cfg = SearchConfig {
            open_subsonic: true,
            ..SearchConfig::default()
        };

        assert!(cfg.enabled_sources().contains(&SearchSource::RadioBrowser));
        assert!(cfg.enabled_sources().contains(&SearchSource::OpenSubsonic));
        assert!(
            !cfg.enabled_public_sources()
                .contains(&SearchSource::OpenSubsonic)
        );
        assert!(
            !cfg.streaming_enabled_sources()
                .contains(&SearchSource::RadioBrowser)
        );
        assert!(
            !cfg.selectable_streaming_sources()
                .contains(&SearchSource::RadioBrowser)
        );
        assert!(
            !cfg.streaming_enabled_sources()
                .contains(&SearchSource::OpenSubsonic)
        );
        assert!(
            !cfg.selectable_streaming_sources()
                .contains(&SearchSource::OpenSubsonic)
        );
    }

    #[test]
    fn radio_browser_streaming_source_normalizes_to_track_source() {
        let cfg = SearchConfig {
            streaming_source: SearchSource::RadioBrowser,
            youtube: false,
            soundcloud: true,
            audius: false,
            jamendo: false,
            internet_archive: false,
            radio_browser: true,
            ..SearchConfig::default()
        }
        .normalized();

        assert_eq!(cfg.streaming_source, SearchSource::SoundCloud);
    }

    #[test]
    fn open_subsonic_is_disabled_by_default_and_selectable_only_when_enabled() {
        let mut cfg = SearchConfig::default();
        assert!(!cfg.open_subsonic);
        assert!(
            !cfg.selectable_sources()
                .contains(&SearchSource::OpenSubsonic)
        );

        cfg.set_enabled(SearchSource::OpenSubsonic, true);
        assert!(
            cfg.selectable_sources()
                .contains(&SearchSource::OpenSubsonic)
        );

        let before = SearchSource::InternetArchive;
        assert_eq!(cfg.cycled_source(before, true), SearchSource::OpenSubsonic);
        assert_eq!(cfg.cycled_source(SearchSource::OpenSubsonic, false), before);

        cfg.source = SearchSource::OpenSubsonic;
        cfg.set_enabled(SearchSource::OpenSubsonic, false);
        assert_eq!(cfg.source, SearchSource::Youtube);
    }

    #[test]
    fn old_search_config_defaults_open_subsonic_to_off() {
        let cfg: SearchConfig = serde_json::from_str("{}").unwrap();

        assert!(!cfg.open_subsonic);
        assert_eq!(cfg.source, SearchSource::Youtube);
    }

    #[test]
    fn open_subsonic_never_normalizes_as_a_streaming_source() {
        let cfg = SearchConfig {
            streaming_source: SearchSource::OpenSubsonic,
            youtube: false,
            soundcloud: true,
            audius: false,
            jamendo: false,
            internet_archive: false,
            open_subsonic: true,
            radio_browser: false,
            ..SearchConfig::default()
        }
        .normalized();

        assert_eq!(cfg.streaming_source, SearchSource::SoundCloud);
        assert!(
            !cfg.streaming_enabled_sources()
                .contains(&SearchSource::OpenSubsonic)
        );
    }
}
