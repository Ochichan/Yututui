use std::path::{Path, PathBuf};

use super::super::{
    FileFormat, ImportMediaKind, JobSpec, MatchPolicy, TransferCacheMode, TransferDest,
    TransferSource,
};

pub(super) struct CommonFlags {
    pub(super) dry_run: bool,
    pub(super) yes: bool,
    pub(super) min_score: f32,
    pub(super) match_policy: Option<MatchPolicy>,
    pub(super) cache_mode: TransferCacheMode,
    pub(super) allow_user_videos: bool,
    pub(super) take_best: bool,
    pub(super) rematch: bool,
}

pub(super) fn parse_common(args: &mut Vec<&str>) -> Result<CommonFlags, String> {
    let mut flags = CommonFlags {
        dry_run: false,
        yes: false,
        min_score: 0.80,
        match_policy: None,
        cache_mode: TransferCacheMode::Use,
        allow_user_videos: false,
        take_best: false,
        rematch: false,
    };
    let mut keep = Vec::new();
    let mut it = args.iter().copied().peekable();
    while let Some(arg) = it.next() {
        match arg {
            "--dry-run" => flags.dry_run = true,
            "--yes" => flags.yes = true,
            "--take-best" => flags.take_best = true,
            "--allow-user-videos" => flags.allow_user_videos = true,
            "--rematch" => flags.rematch = true,
            "--policy" => {
                let v = it.next().ok_or("--policy needs a value")?;
                flags.match_policy = Some(v.parse()?);
            }
            "--cache" => {
                let v = it.next().ok_or("--cache needs a value")?;
                flags.cache_mode = v.parse()?;
            }
            "--min-score" => {
                let v = it.next().ok_or("--min-score needs a value")?;
                let score: f32 = v.parse().map_err(|_| format!("bad --min-score `{v}`"))?;
                if !(0.0..=1.0).contains(&score) {
                    return Err(format!("--min-score must be 0..1 (got {score})"));
                }
                flags.min_score = score;
            }
            other => keep.push(other),
        }
    }
    *args = keep;
    Ok(flags)
}

/// A Spotify playlist reference in any of the shapes people paste.
pub(super) fn parse_spotify_playlist_id(s: &str) -> Option<String> {
    if let Some(rest) = s.strip_prefix("spotify:playlist:") {
        return Some(rest.to_owned());
    }
    if let Some(idx) = s.find("open.spotify.com/playlist/") {
        let rest = &s[idx + "open.spotify.com/playlist/".len()..];
        let id: String = rest
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric())
            .collect();
        return (!id.is_empty()).then_some(id);
    }
    (s.len() == 22 && s.bytes().all(|b| b.is_ascii_alphanumeric())).then(|| s.to_owned())
}

pub(super) fn parse_import(args: &[&str]) -> Result<(JobSpec, bool), String> {
    let mut args: Vec<&str> = args.to_vec();
    let flags = parse_common(&mut args)?;
    let mut source_arg = None;
    let mut dest = None;
    let mut media_kind = ImportMediaKind::Track;
    let mut it = args.iter().copied().peekable();
    while let Some(arg) = it.next() {
        match arg {
            "--media" => {
                media_kind = match it.next().ok_or("--media needs `track` or `music-video`")? {
                    "track" => ImportMediaKind::Track,
                    "music-video" => ImportMediaKind::MusicVideo,
                    other => {
                        return Err(format!(
                            "--media expects `track` or `music-video` (got `{other}`)"
                        ));
                    }
                };
            }
            "--to-playlist" => {
                let name = it.next().ok_or("--to-playlist needs a name")?;
                dest = Some(TransferDest::YtmExistingPlaylist {
                    name: name.to_owned(),
                });
            }
            "--to" => match it.next() {
                Some("likes") => dest = Some(TransferDest::YtmLikes),
                Some("local") => dest = Some(TransferDest::LocalPlaylist { name: None }),
                Some(other) if other.starts_with("local:") => {
                    dest = Some(TransferDest::LocalPlaylist {
                        name: Some(other["local:".len()..].to_owned())
                            .filter(|n| !n.trim().is_empty()),
                    });
                }
                other => {
                    return Err(format!(
                        "--to expects `likes`, `local`, or `local:NAME` (got {other:?})"
                    ));
                }
            },
            other if source_arg.is_none() => source_arg = Some(other.to_owned()),
            other => return Err(format!("unexpected argument `{other}`")),
        }
    }
    let source_arg = source_arg.ok_or("missing source (Spotify URL/id, `liked`, or a file)")?;
    // Deterministic source resolution: existing file → file import; `liked`; else it
    // must parse as a Spotify playlist reference.
    let source = if Path::new(&source_arg).is_file() {
        TransferSource::File {
            path: PathBuf::from(&source_arg),
        }
    } else if source_arg.eq_ignore_ascii_case("liked") {
        TransferSource::SpotifyLiked
    } else if let Some(id) = parse_spotify_playlist_id(&source_arg) {
        TransferSource::SpotifyPlaylist { id }
    } else {
        return Err(format!(
            "`{source_arg}` is not an existing file, `liked`, or a Spotify playlist URL/URI/id"
        ));
    };
    let dest = dest.unwrap_or(TransferDest::YtmNewPlaylist { name: None });
    if media_kind == ImportMediaKind::MusicVideo {
        if matches!(source, TransferSource::File { .. }) {
            return Err(
                "--media music-video currently requires a Spotify playlist or `liked` source"
                    .to_owned(),
            );
        }
        if matches!(dest, TransferDest::YtmLikes) {
            return Err(
                "--media music-video requires a playlist destination, not `--to likes`".to_owned(),
            );
        }
        if flags.allow_user_videos {
            return Err(
                "--allow-user-videos conflicts with official-family music-video mode".to_owned(),
            );
        }
    }
    Ok((
        JobSpec {
            source,
            dest,
            media_kind,
            dry_run: flags.dry_run,
            min_score: flags.min_score,
            take_best: flags.take_best,
            auto_accept_ambiguous_min_score: None,
            match_policy: flags.match_policy.unwrap_or(
                if media_kind == ImportMediaKind::MusicVideo {
                    MatchPolicy::Strict
                } else {
                    MatchPolicy::Balanced
                },
            ),
            allow_user_videos: flags.allow_user_videos,
            cache_mode: flags.cache_mode,
            rematch: flags.rematch,
        },
        flags.yes,
    ))
}

pub(super) fn parse_ytm_source(s: &str) -> Result<TransferSource, String> {
    if let Some(id) = s.strip_prefix("ytm:") {
        Ok(TransferSource::YtmPlaylist { id: id.to_owned() })
    } else if let Some(key) = s.strip_prefix("local:") {
        Ok(TransferSource::LocalPlaylist {
            key: key.to_owned(),
        })
    } else {
        Err(format!(
            "`{s}` — expected `ytm:<playlist-id>` (see `ytt transfer list ytm`) or `local:<name>`"
        ))
    }
}

pub(super) fn parse_export(args: &[&str]) -> Result<(JobSpec, bool), String> {
    let mut args: Vec<&str> = args.to_vec();
    let flags = parse_common(&mut args)?;
    let mut source = None;
    enum ExportTarget {
        SpotifyAppend,
        SpotifyMirror(String),
        File(PathBuf),
    }
    let mut dest = None;
    let mut name = None;
    let mut sync = false;
    let mut it = args.iter().copied().peekable();
    while let Some(arg) = it.next() {
        match arg {
            "--to" => match it.next() {
                Some("spotify") => dest = Some(ExportTarget::SpotifyAppend),
                Some(value) if value.starts_with("spotify:") => {
                    let id = &value["spotify:".len()..];
                    if parse_spotify_playlist_id(id).as_deref() != Some(id) {
                        return Err(format!(
                            "`{value}` is not `spotify:<22-character-playlist-id>`"
                        ));
                    }
                    dest = Some(ExportTarget::SpotifyMirror(id.to_owned()));
                }
                Some(path) => dest = Some(ExportTarget::File(PathBuf::from(path))),
                None => return Err("--to needs `spotify` or a file path".to_owned()),
            },
            "--sync" => sync = true,
            "--name" => name = Some(it.next().ok_or("--name needs a value")?.to_owned()),
            other if source.is_none() => source = Some(parse_ytm_source(other)?),
            other => return Err(format!("unexpected argument `{other}`")),
        }
    }
    let source = source.ok_or("missing source (`ytm:<id>` or `local:<key>`)")?;
    let dest = match dest.ok_or("missing --to (spotify | spotify:ID | FILE.json | FILE.csv)")? {
        ExportTarget::SpotifyAppend => {
            if sync {
                return Err(
                    "--sync requires an explicit `--to spotify:<playlist-id>` target".to_owned(),
                );
            }
            TransferDest::SpotifyNewPlaylist { name }
        }
        ExportTarget::SpotifyMirror(id) => {
            if !sync {
                return Err(
                    "`--to spotify:<playlist-id>` requires the explicit `--sync` flag".to_owned(),
                );
            }
            if name.is_some() {
                return Err("--name cannot be used with an ID-targeted Spotify sync".to_owned());
            }
            TransferDest::SpotifyMirrorPlaylist { id }
        }
        ExportTarget::File(path) => {
            if sync {
                return Err("--sync is only valid with `--to spotify:<playlist-id>`".to_owned());
            }
            if name.is_some() {
                return Err("--name is only valid with `--to spotify`".to_owned());
            }
            let format = match path
                .extension()
                .and_then(|e| e.to_str())
                .map(str::to_ascii_lowercase)
                .as_deref()
            {
                Some("json") => FileFormat::Json,
                Some("csv") => FileFormat::Csv,
                other => {
                    return Err(format!(
                        "--to file must end in .json or .csv (got {other:?})"
                    ));
                }
            };
            TransferDest::File { path, format }
        }
    };
    Ok((
        JobSpec {
            source,
            dest,
            media_kind: ImportMediaKind::Track,
            dry_run: flags.dry_run,
            min_score: flags.min_score,
            take_best: flags.take_best,
            auto_accept_ambiguous_min_score: None,
            match_policy: flags.match_policy.unwrap_or(MatchPolicy::Strict),
            allow_user_videos: flags.allow_user_videos,
            cache_mode: flags.cache_mode,
            rematch: flags.rematch,
        },
        flags.yes,
    ))
}

pub(super) fn parse_backup(args: &[&str]) -> Result<(PathBuf, bool), String> {
    let mut dir = None;
    let mut csv = false;
    let mut it = args.iter().copied();
    while let Some(arg) = it.next() {
        match arg {
            "--dir" => dir = Some(PathBuf::from(it.next().ok_or("--dir needs a path")?)),
            "--csv" => csv = true,
            other => return Err(format!("unexpected argument `{other}`")),
        }
    }
    Ok((dir.ok_or("missing --dir DIR")?, csv))
}
