use anyhow::{Result, bail};

use super::{Song, archive_search, audius_search, jamendo_search, ytdlp_flat_search, ytdlp_search};
use crate::search_source::{SearchConfig, SearchSource};

pub(super) async fn search(
    source: SearchSource,
    query: &str,
    config: &SearchConfig,
    limit: usize,
) -> Result<Vec<Song>> {
    match source {
        SearchSource::SoundCloud => {
            ytdlp_flat_search(SearchSource::SoundCloud, "scsearch", query, limit).await
        }
        SearchSource::Audius => audius_search(query, config, limit).await,
        SearchSource::Jamendo => jamendo_search(query, config, limit).await,
        SearchSource::InternetArchive => archive_search(query, limit).await,
        SearchSource::Youtube => ytdlp_search(query, limit).await,
        SearchSource::OpenSubsonic | SearchSource::RadioBrowser | SearchSource::All => {
            bail!("{} is not a track recommendation source", source.label())
        }
    }
}
