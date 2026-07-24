use std::collections::{BTreeMap, HashMap, VecDeque};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{
    CausalStamp, DeviceId, DeviceRecord, Dot, Operation, OperationEnvelope, OperationOrigin,
    PersonalStateError, PersonalStateV2, PlaylistEntryId, PlaylistId, PortableTrack,
    PortableTrackKey, Rating, VersionVector,
};

pub(crate) const FAVORITES_MAX: usize = 999;
pub(crate) const HISTORY_MAX: usize = 999;
pub(crate) const RADIO_MAX: usize = 999;
pub(crate) const PLAYLISTS_MAX: usize = 999;
pub(crate) const PLAYLIST_ENTRIES_MAX: usize = 999;
pub(crate) const SIGNAL_TRACKS_MAX: usize = 5_000;
pub(crate) const ENGAGEMENT_EVENTS_MAX: usize = 20_000;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct LegacyProjection {
    #[serde(default)]
    pub favorites: Vec<PortableTrack>,
    #[serde(default)]
    pub history: Vec<PortableTrack>,
    #[serde(default)]
    pub radio_favorites: Vec<PortableTrack>,
    #[serde(default)]
    pub radio_history: Vec<PortableTrack>,
    #[serde(default)]
    pub playlists: Vec<LegacyPlaylist>,
    #[serde(default)]
    pub signals: LegacySignals,
    #[serde(default)]
    pub station: LegacyStation,
}

impl LegacyProjection {
    pub fn validate(&self) -> Result<(), PersonalStateError> {
        if self.favorites.len() > FAVORITES_MAX.saturating_mul(4)
            || self.history.len() > HISTORY_MAX.saturating_mul(4)
            || self.radio_favorites.len() > RADIO_MAX.saturating_mul(4)
            || self.radio_history.len() > RADIO_MAX.saturating_mul(4)
            || self.playlists.len() > PLAYLISTS_MAX.saturating_mul(4)
            || self.signals.tracks.len() > SIGNAL_TRACKS_MAX.saturating_mul(4)
            || self.signals.play_log.len() > ENGAGEMENT_EVENTS_MAX.saturating_mul(4)
        {
            return Err(PersonalStateError::InvalidOperation(
                "legacy baseline exceeds collection limits",
            ));
        }
        for track in self
            .favorites
            .iter()
            .chain(&self.history)
            .chain(&self.radio_favorites)
            .chain(&self.radio_history)
        {
            track.validate()?;
        }
        for playlist in &self.playlists {
            playlist.validate()?;
        }
        for (key, signal) in &self.signals.tracks {
            key.validate()?;
            signal.track.validate()?;
            if !signal.last_completion.is_finite() || !(0.0..=1.0).contains(&signal.last_completion)
            {
                return Err(PersonalStateError::InvalidOperation(
                    "legacy completion is not finite",
                ));
            }
        }
        if self
            .signals
            .artist_affinity
            .values()
            .any(|weight| !weight.is_finite())
        {
            return Err(PersonalStateError::InvalidOperation(
                "legacy artist affinity is not finite",
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LegacyPlaylist {
    pub playlist_id: PlaylistId,
    pub slug: String,
    pub name: String,
    #[serde(default)]
    pub entries: Vec<LegacyPlaylistEntry>,
}

impl LegacyPlaylist {
    fn validate(&self) -> Result<(), PersonalStateError> {
        super::model::validate_id("playlist id", self.playlist_id.as_str(), 512)?;
        super::model::validate_id("playlist slug", &self.slug, 512)?;
        super::model::validate_text("playlist name", &self.name)?;
        if self.entries.len() > PLAYLIST_ENTRIES_MAX.saturating_mul(4) {
            return Err(PersonalStateError::InvalidOperation(
                "legacy playlist exceeds entry limit",
            ));
        }
        for entry in &self.entries {
            super::model::validate_id("playlist entry id", entry.entry_id.as_str(), 512)?;
            entry.track.validate()?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LegacyPlaylistEntry {
    pub entry_id: PlaylistEntryId,
    pub track: PortableTrack,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct LegacySignals {
    #[serde(default, with = "track_signal_map")]
    pub tracks: BTreeMap<PortableTrackKey, LegacyTrackSignal>,
    #[serde(default)]
    pub artist_affinity: BTreeMap<String, f32>,
    #[serde(default)]
    pub play_log: Vec<LegacyPlayEvent>,
}

mod track_signal_map {
    use std::collections::BTreeMap;

    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    use super::{LegacyTrackSignal, PortableTrackKey};

    pub fn serialize<S>(
        value: &BTreeMap<PortableTrackKey, LegacyTrackSignal>,
        serializer: S,
    ) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        value.iter().collect::<Vec<_>>().serialize(serializer)
    }

    pub fn deserialize<'de, D>(
        deserializer: D,
    ) -> Result<BTreeMap<PortableTrackKey, LegacyTrackSignal>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let entries = Vec::<(PortableTrackKey, LegacyTrackSignal)>::deserialize(deserializer)?;
        let mut value = BTreeMap::new();
        for (key, signal) in entries {
            if value.insert(key, signal).is_some() {
                return Err(serde::de::Error::custom(
                    "duplicate portable track signal key",
                ));
            }
        }
        Ok(value)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LegacyTrackSignal {
    pub track: PortableTrack,
    pub play_count: u32,
    pub completed_count: u32,
    pub skip_count: u32,
    pub last_played_at: i64,
    pub last_completion: f32,
    #[serde(default)]
    pub disliked: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LegacyPlayEvent {
    pub event_id: String,
    pub track: PortableTrack,
    pub played_at: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LegacyStation {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query: Option<String>,
    #[serde(default)]
    pub explore: crate::station::Explore,
    #[serde(default)]
    pub avoid_artist_keys: Vec<String>,
}

/// Build the one-time v2 baseline from the four existing runtime projections.
pub fn legacy_state(
    library: &crate::library::Library,
    playlists: &crate::playlists::Playlists,
    signals: &crate::signals::Signals,
    station: &crate::station::StationStore,
) -> Result<PersonalStateV2, PersonalStateError> {
    let projection = LegacyProjection::from_runtime(library, playlists, signals, station);
    legacy_state_from_projection(projection)
}

pub(crate) fn legacy_state_from_projection(
    projection: LegacyProjection,
) -> Result<PersonalStateV2, PersonalStateError> {
    projection.validate()?;
    let baseline_bytes = serde_json::to_vec(&projection)?;
    let baseline_hash = sha256_hex(&baseline_bytes);
    let dataset_id = format!("ds-{}", &baseline_hash[..32]);
    let legacy_device = DeviceId::new("legacy")?;
    let stamp = CausalStamp {
        dot: Dot {
            device_id: legacy_device.clone(),
            sequence: 1,
        },
        observed: VersionVector::default(),
        recorded_at_unix: 0,
    };
    let mut state = PersonalStateV2::empty(dataset_id)?;
    state.operations.push(OperationEnvelope {
        operation_id: format!("legacy-{baseline_hash}"),
        stamp: stamp.clone(),
        origin: OperationOrigin::Imported,
        operation: Operation::LegacyBaseline {
            baseline: Box::new(projection),
        },
    });
    state.version_vector.observe(&stamp.dot);
    let local_device = DeviceId::new(format!("local-{}", &baseline_hash[..24]))?;
    let mut observed = VersionVector::default();
    observed.observe(&stamp.dot);
    let device_stamp = CausalStamp {
        dot: Dot {
            device_id: local_device.clone(),
            sequence: 1,
        },
        observed,
        recorded_at_unix: 0,
    };
    let device = DeviceRecord {
        device_id: local_device.clone(),
        name: "This device".to_owned(),
        revoked: false,
        public_identity: None,
    };
    state.device_registry.insert(local_device, device.clone());
    state.operations.push(OperationEnvelope {
        operation_id: format!("device-{}", &baseline_hash[..32]),
        stamp: device_stamp.clone(),
        origin: OperationOrigin::Local,
        operation: Operation::AddDevice { device },
    });
    state.version_vector.observe(&device_stamp.dot);
    state.normalize()?;
    Ok(state)
}

impl LegacyProjection {
    pub(crate) fn from_runtime(
        library: &crate::library::Library,
        playlists: &crate::playlists::Playlists,
        signals: &crate::signals::Signals,
        station: &crate::station::StationStore,
    ) -> Self {
        let mut catalog = HashMap::<String, PortableTrack>::new();
        let favorites = portable_tracks(&library.favorites, &mut catalog);
        let history = portable_tracks(library.history.iter(), &mut catalog);
        let radio_favorites = portable_tracks(&library.radio_favorites, &mut catalog);
        let radio_history = portable_tracks(library.radios.iter(), &mut catalog);

        let playlists = playlists
            .list()
            .iter()
            .take(PLAYLISTS_MAX)
            .enumerate()
            .map(|(playlist_index, playlist)| {
                let playlist_id = PlaylistId(format!(
                    "legacy-playlist-{}",
                    stable_hash(&format!(
                        "{}\u{0}{}\u{0}{playlist_index}",
                        playlist.id, playlist.name
                    ))
                ));
                let entries = playlist
                    .songs
                    .iter()
                    .take(PLAYLIST_ENTRIES_MAX)
                    .enumerate()
                    .map(|(entry_index, song)| {
                        let track = portable_track(song);
                        catalog.insert(song.video_id.clone(), track.clone());
                        LegacyPlaylistEntry {
                            entry_id: PlaylistEntryId(format!(
                                "legacy-entry-{}",
                                stable_hash(&format!(
                                    "{}\u{0}{entry_index}\u{0}{:?}",
                                    playlist_id.0, track.key
                                ))
                            )),
                            track,
                        }
                    })
                    .collect();
                LegacyPlaylist {
                    playlist_id,
                    slug: playlist.id.clone(),
                    name: playlist.name.clone(),
                    entries,
                }
            })
            .collect();

        let signals = signals.personal_state_legacy_signals(&catalog);
        let station = station
            .active
            .as_ref()
            .map_or_else(LegacyStation::default, |profile| LegacyStation {
                query: (!profile.query.is_empty()).then(|| profile.query.clone()),
                explore: profile.explore,
                avoid_artist_keys: profile.avoid_artist_keys.clone(),
            });

        Self {
            favorites,
            history,
            radio_favorites,
            radio_history,
            playlists,
            signals,
            station,
        }
    }

    pub(crate) fn into_runtime(
        self,
    ) -> (
        crate::library::Library,
        crate::playlists::Playlists,
        crate::signals::Signals,
        crate::station::StationStore,
    ) {
        let library = crate::library::Library {
            favorites: self
                .favorites
                .into_iter()
                .take(FAVORITES_MAX)
                .map(portable_track_to_song)
                .collect(),
            history: self
                .history
                .into_iter()
                .take(HISTORY_MAX)
                .map(portable_track_to_song)
                .collect::<VecDeque<_>>(),
            radio_favorites: self
                .radio_favorites
                .into_iter()
                .take(RADIO_MAX)
                .map(portable_track_to_song)
                .collect(),
            radios: self
                .radio_history
                .into_iter()
                .take(RADIO_MAX)
                .map(portable_track_to_song)
                .collect::<VecDeque<_>>(),
            ..crate::library::Library::default()
        };
        let playlists = crate::playlists::Playlists {
            playlists: self
                .playlists
                .into_iter()
                .take(PLAYLISTS_MAX)
                .map(|playlist| crate::playlists::Playlist {
                    id: playlist.slug,
                    name: playlist.name,
                    songs: playlist
                        .entries
                        .into_iter()
                        .take(PLAYLIST_ENTRIES_MAX)
                        .map(|entry| portable_track_to_song(entry.track))
                        .collect(),
                })
                .collect(),
            ..crate::playlists::Playlists::default()
        };
        let signals = crate::signals::Signals::from_personal_state_legacy(self.signals);
        let station = crate::station::StationStore {
            active: self
                .station
                .query
                .map(|query| crate::station::StationProfile {
                    query,
                    explore: self.station.explore,
                    avoid_artist_keys: self.station.avoid_artist_keys,
                }),
        };
        (library, playlists, signals, station)
    }
}

pub(crate) fn portable_track(song: &crate::api::Song) -> PortableTrack {
    let key = if let Some(youtube_id) = song.youtube_id() {
        PortableTrackKey::Catalog {
            provider: "youtube".to_owned(),
            exact_catalog_id: youtube_id.to_owned(),
        }
    } else if song.local_path.is_some() {
        PortableTrackKey::LocalPlaceholder {
            portable_placeholder_id: stable_hash(&format!(
                "{}\u{0}{}\u{0}{}\u{0}{:?}",
                song.title, song.artist, song.duration, song.album
            )),
        }
    } else {
        PortableTrackKey::Catalog {
            provider: song.source.id_prefix().to_owned(),
            exact_catalog_id: song.video_id.clone(),
        }
    };
    PortableTrack {
        key,
        title: song.title.clone(),
        artist: song.artist.clone(),
        album: song.album.clone(),
        duration_secs: song
            .duration_secs
            .or_else(|| crate::streaming::candidate::parse_duration_secs(&song.duration)),
        isrc: song.isrc.clone(),
    }
}

pub(crate) fn portable_track_to_song(track: PortableTrack) -> crate::api::Song {
    let (video_id, source) = match &track.key {
        PortableTrackKey::Catalog {
            provider,
            exact_catalog_id,
        } if provider == "youtube" || provider == "yt" => (
            exact_catalog_id.clone(),
            crate::search_source::SearchSource::Youtube,
        ),
        PortableTrackKey::Catalog {
            provider,
            exact_catalog_id,
        } => (
            exact_catalog_id.clone(),
            source_from_provider(provider).unwrap_or_default(),
        ),
        PortableTrackKey::OpenSubsonic {
            backend_id,
            account_scope_id,
            item_id,
        } => (
            format!(
                "subsonic:{}",
                stable_hash(&format!(
                    "{backend_id}\u{0}{account_scope_id}\u{0}{item_id}"
                ))
            ),
            crate::search_source::SearchSource::Youtube,
        ),
        PortableTrackKey::LocalPlaceholder {
            portable_placeholder_id,
        } => (
            format!("local-placeholder:{portable_placeholder_id}"),
            crate::search_source::SearchSource::Youtube,
        ),
    };
    let duration = track.duration_secs.map_or_else(String::new, |seconds| {
        format!("{}:{:02}", seconds / 60, seconds % 60)
    });
    let mut song = crate::api::Song::remote(video_id, track.title, track.artist, duration);
    song.source = source;
    song.album = track.album;
    song.duration_secs = track.duration_secs;
    song.isrc = track.isrc;
    song
}

fn source_from_provider(provider: &str) -> Option<crate::search_source::SearchSource> {
    use crate::search_source::SearchSource;
    match provider {
        "sc" | "soundcloud" => Some(SearchSource::SoundCloud),
        "au" | "audius" => Some(SearchSource::Audius),
        "ja" | "jamendo" => Some(SearchSource::Jamendo),
        "ia" | "internet_archive" => Some(SearchSource::InternetArchive),
        "rad" | "radio_browser" => Some(SearchSource::RadioBrowser),
        _ => None,
    }
}

fn portable_tracks<'a>(
    songs: impl IntoIterator<Item = &'a crate::api::Song>,
    catalog: &mut HashMap<String, PortableTrack>,
) -> Vec<PortableTrack> {
    songs
        .into_iter()
        .take(HISTORY_MAX)
        .map(|song| {
            let track = portable_track(song);
            catalog.insert(song.video_id.clone(), track.clone());
            track
        })
        .collect()
}

pub(crate) fn rating_from_legacy(
    favorites: &[PortableTrack],
    signals: &LegacySignals,
) -> BTreeMap<PortableTrackKey, (PortableTrack, Rating)> {
    let mut ratings = BTreeMap::new();
    for track in favorites {
        ratings.insert(track.key.clone(), (track.clone(), Rating::Liked));
    }
    for signal in signals.tracks.values().filter(|signal| signal.disliked) {
        // A contradictory legacy state resolves to Disliked.
        ratings.insert(
            signal.track.key.clone(),
            (signal.track.clone(), Rating::Disliked),
        );
    }
    ratings
}

pub(crate) fn stable_hash(value: &str) -> String {
    sha256_hex(value.as_bytes())[..32].to_owned()
}

pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(digest.len() * 2);
    use std::fmt::Write as _;
    for byte in digest {
        let _ = write!(out, "{byte:02x}");
    }
    out
}
