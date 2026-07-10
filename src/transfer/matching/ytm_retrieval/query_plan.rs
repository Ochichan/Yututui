use std::collections::HashSet;

use crate::transfer::ImportMediaKind;

use super::super::{
    TrackInput, normalize, normalize_stripped, push_query_variant, release_year, strip_annotations,
};

/// Memo key: repeated instances of the same source recording resolve once per engine run,
/// without aliasing different versions, releases, media intents, or source identities.
pub fn memo_key(input: &TrackInput) -> String {
    memo_key_for_media(input, ImportMediaKind::Track)
}

pub fn memo_key_for_media(input: &TrackInput, media_kind: ImportMediaKind) -> String {
    let artists = input
        .artists
        .iter()
        .map(|artist| normalize(artist))
        .collect::<Vec<_>>()
        .join("\u{1f}");
    format!(
        "media={}|source={}|isrc={}|artists={artists}|title={}|stripped={}|album={}|duration={}|album_id={}|disc={}|track={}",
        media_scope(media_kind),
        normalize(&input.source_key),
        input
            .isrc
            .as_deref()
            .map(str::trim)
            .unwrap_or_default()
            .to_ascii_uppercase(),
        normalize(&input.title),
        normalize_stripped(&input.title),
        input.album.as_deref().map(normalize).unwrap_or_default(),
        input
            .duration_secs
            .map_or_else(String::new, |value| value.to_string()),
        input.album_id.as_deref().map(normalize).unwrap_or_default(),
        input
            .disc_number
            .map_or_else(String::new, |value| value.to_string()),
        input
            .track_number
            .map_or_else(String::new, |value| value.to_string()),
    )
}

pub(super) fn media_scope(media_kind: ImportMediaKind) -> &'static str {
    match media_kind {
        ImportMediaKind::Track => "track",
        ImportMediaKind::MusicVideo => "music_video",
    }
}

/// Build the bounded YTM query plan for a source track.
pub fn ytm_query_plan(input: &TrackInput) -> Vec<String> {
    let stripped_title = strip_annotations(&input.title);
    let stripped_title = stripped_title.trim();
    let original_title = input.title.trim();
    let mut out = Vec::new();
    let mut seen = HashSet::new();

    let first_artist = input
        .artists
        .first()
        .map(|artist| artist.trim())
        .filter(|artist| !artist.is_empty());
    if let Some(artist) = first_artist {
        push_query_variant(&mut out, &mut seen, format!("{artist} {stripped_title}"));
    }

    let all_artists = input
        .artists
        .iter()
        .map(|artist| artist.trim())
        .filter(|artist| !artist.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    if !all_artists.is_empty() {
        push_query_variant(
            &mut out,
            &mut seen,
            format!("{all_artists} {stripped_title}"),
        );
        if normalize(original_title) != normalize(stripped_title) {
            push_query_variant(
                &mut out,
                &mut seen,
                format!("{all_artists} {original_title}"),
            );
        }
    }

    let album_artists = input
        .album_artists
        .iter()
        .map(|artist| artist.trim())
        .filter(|artist| !artist.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    if !album_artists.is_empty() {
        push_query_variant(
            &mut out,
            &mut seen,
            format!("{album_artists} {stripped_title}"),
        );
    }

    if let Some(album) = input
        .album
        .as_deref()
        .map(str::trim)
        .filter(|album| !album.is_empty())
        && normalize(album) != normalize(stripped_title)
    {
        if let Some(artist) = first_artist {
            push_query_variant(
                &mut out,
                &mut seen,
                format!("{artist} {stripped_title} {album}"),
            );
        }
        push_query_variant(&mut out, &mut seen, format!("{stripped_title} {album}"));
    }

    if let Some(year) = release_year(input) {
        if let Some(artist) = first_artist {
            push_query_variant(
                &mut out,
                &mut seen,
                format!("{artist} {stripped_title} {year}"),
            );
        }
        if !all_artists.is_empty() {
            push_query_variant(
                &mut out,
                &mut seen,
                format!("{all_artists} {stripped_title} {year}"),
            );
        }
    }

    if let Some(artist) = first_artist {
        push_query_variant(
            &mut out,
            &mut seen,
            format!("{artist} {stripped_title} official audio"),
        );
        push_query_variant(
            &mut out,
            &mut seen,
            format!("{artist} {stripped_title} topic"),
        );
    }

    if normalize(original_title) != normalize(stripped_title) {
        push_query_variant(&mut out, &mut seen, original_title.to_owned());
    }
    push_query_variant(&mut out, &mut seen, stripped_title.to_owned());
    out
}

pub fn ytm_catalog_query_plan(input: &TrackInput) -> Vec<String> {
    let stripped_title = strip_annotations(&input.title);
    let stripped_title = stripped_title.trim();
    let original_title = input.title.trim();
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    if let Some(artist) = input
        .artists
        .first()
        .map(|artist| artist.trim())
        .filter(|artist| !artist.is_empty())
    {
        push_query_variant(&mut out, &mut seen, format!("{artist} {stripped_title}"));
        if normalize(original_title) != normalize(stripped_title) {
            push_query_variant(&mut out, &mut seen, format!("{artist} {original_title}"));
        }
        push_query_variant(&mut out, &mut seen, format!("{stripped_title} {artist}"));
    } else {
        push_query_variant(&mut out, &mut seen, stripped_title.to_owned());
    }
    out
}

/// Video-first queries used by the explicit music-video import mode.
pub fn ytm_music_video_query_plan(input: &TrackInput) -> Vec<String> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    let stripped_title = strip_annotations(&input.title);
    let stripped_title = stripped_title.trim();
    let original_title = input.title.trim();
    let artist = input
        .artists
        .first()
        .map(|artist| artist.trim())
        .filter(|artist| !artist.is_empty());

    if let Some(artist) = artist {
        push_query_variant(
            &mut out,
            &mut seen,
            format!("{artist} {stripped_title} official music video"),
        );
        push_query_variant(
            &mut out,
            &mut seen,
            format!("{artist} {stripped_title} official video"),
        );
        if normalize(original_title) != normalize(stripped_title) {
            push_query_variant(
                &mut out,
                &mut seen,
                format!("{artist} {original_title} music video"),
            );
        }
    }
    push_query_variant(
        &mut out,
        &mut seen,
        format!("{stripped_title} official music video"),
    );
    out
}

/// Build the three-query public YouTube rescue plan.
pub fn ytm_fallback_query_plan(input: &TrackInput) -> Vec<String> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    let stripped_title = strip_annotations(&input.title);
    let stripped_title = stripped_title.trim();
    let first_artist = input
        .artists
        .first()
        .map(|artist| artist.trim())
        .filter(|artist| !artist.is_empty());

    if let Some(artist) = first_artist {
        let base = format!("{artist} {stripped_title}");
        push_query_variant(&mut out, &mut seen, base.clone());
        push_query_variant(&mut out, &mut seen, format!("{base} official audio"));
        push_query_variant(
            &mut out,
            &mut seen,
            format!("{stripped_title} official music video"),
        );
    } else {
        let base = input
            .album
            .as_deref()
            .map(str::trim)
            .filter(|album| !album.is_empty())
            .map_or_else(
                || stripped_title.to_owned(),
                |album| format!("{stripped_title} {album}"),
            );
        push_query_variant(&mut out, &mut seen, base);
        push_query_variant(
            &mut out,
            &mut seen,
            format!("{stripped_title} official audio"),
        );
        push_query_variant(
            &mut out,
            &mut seen,
            format!("{stripped_title} official music video"),
        );
    }
    out
}
