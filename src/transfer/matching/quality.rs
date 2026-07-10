use crate::api::ytmusic::YtmMusicVideoType;
use crate::streaming::musicgate;

use super::{CandidateSourceKind, MatchCandidate};

pub(super) fn official_signal_score(cand: &MatchCandidate) -> f32 {
    match cand.music_video_type {
        Some(YtmMusicVideoType::Omv) => return 1.0,
        Some(YtmMusicVideoType::OfficialSourceMusic) => return 0.9,
        _ => {}
    }
    if matches!(
        cand.source_kind,
        CandidateSourceKind::YtmAlbumTrack
            | CandidateSourceKind::YtmCatalogSong
            | CandidateSourceKind::SpotifyCatalog
    ) {
        return 1.0;
    }
    let channel = cand.channel.as_deref().unwrap_or(&cand.artist);
    if musicgate::is_trusted_music_channel(channel) {
        return 1.0;
    }
    let title_tier = cand
        .metadata_title
        .as_deref()
        .map(|title| musicgate::music_tier_score(title, channel))
        .unwrap_or(0.0)
        .max(musicgate::music_tier_score(&cand.title, channel));
    if title_tier >= 0.7 {
        return title_tier;
    }
    let channel_lower = channel.to_lowercase();
    if channel_lower.contains("official") || channel_lower.ends_with(" records") {
        return 0.7;
    }
    title_tier
}

pub(super) fn quality_tier(cand: &MatchCandidate) -> &'static str {
    match cand.source_kind {
        CandidateSourceKind::YtmAlbumTrack => "album_track",
        CandidateSourceKind::YtmCatalogSong | CandidateSourceKind::SpotifyCatalog => "catalog",
        CandidateSourceKind::YtmCatalogVideo
        | CandidateSourceKind::YoutubeVideoSearch
        | CandidateSourceKind::Unknown => {
            let official = official_signal_score(cand);
            if official >= 1.0 {
                "trusted_official"
            } else if official >= 0.7 {
                "official_like"
            } else if official >= 0.4 {
                "music_signal"
            } else {
                "unverified_upload"
            }
        }
    }
}

pub(super) fn quality_adjustment(cand: &MatchCandidate) -> (f32, f32, Vec<&'static str>) {
    let mut bonus = 0.0f32;
    let mut penalty = 0.0f32;
    let mut reasons = Vec::new();
    let channel = cand.channel.as_deref().unwrap_or(&cand.artist);

    match cand.source_kind {
        CandidateSourceKind::YtmAlbumTrack
        | CandidateSourceKind::YtmCatalogSong
        | CandidateSourceKind::SpotifyCatalog => {
            bonus += 0.035;
            reasons.push("catalog_song");
            if cand.source_kind == CandidateSourceKind::YtmAlbumTrack {
                bonus += 0.015;
                reasons.push("album_track");
            }
        }
        CandidateSourceKind::YtmCatalogVideo => {
            bonus += 0.01;
            reasons.push("ytm_video_filter");
            let reason = match cand.music_video_type {
                Some(YtmMusicVideoType::Omv) => {
                    bonus += 0.02;
                    "music_video_type_omv"
                }
                Some(YtmMusicVideoType::OfficialSourceMusic) => {
                    bonus += 0.015;
                    "music_video_type_official_source_music"
                }
                Some(YtmMusicVideoType::Ugc) => "music_video_type_ugc",
                Some(YtmMusicVideoType::Atv) => "music_video_type_atv",
                Some(YtmMusicVideoType::Shoulder) => "music_video_type_shoulder",
                Some(YtmMusicVideoType::Episode) => "music_video_type_episode",
                Some(YtmMusicVideoType::Upload) => "music_video_type_upload",
                Some(YtmMusicVideoType::Unknown) | None => "music_video_type_unknown",
            };
            reasons.push(reason);
        }
        CandidateSourceKind::YoutubeVideoSearch | CandidateSourceKind::Unknown => {}
    }

    let tier = musicgate::music_tier_score(&cand.title, channel);
    if tier >= 1.0 {
        bonus += 0.025;
        reasons.push("trusted_channel");
    } else if tier >= 0.7 {
        bonus += 0.015;
        reasons.push("official_like");
    } else if tier >= 0.4 {
        bonus += 0.005;
        reasons.push("music_video_signal");
    }
    if cand.channel_verified == Some(true) && cand.channel_id.is_some() {
        bonus += 0.005;
        reasons.push("verified_channel_corroboration");
    }
    if cand.has_audio_format == Some(true) {
        bonus += 0.005;
        reasons.push("playable_audio_format");
    }

    let risk = musicgate::non_music_risk_score(&cand.title, channel);
    if risk >= 0.85 {
        penalty += 0.35;
        reasons.push("non_music_risk");
    } else if risk >= 0.55 {
        penalty += 0.15;
        reasons.push("non_music_demote");
    }
    if let Some(reason) = musicgate::gimmick_reason(&cand.title) {
        penalty += 0.35;
        reasons.push(reason);
    }

    (bonus, penalty, reasons)
}

pub(super) fn confidence_tier(
    cand: &MatchCandidate,
    total: f32,
    title: f32,
    artist: f32,
    duration_delta: Option<u32>,
    accept_blocked: bool,
    reject_reason: Option<&str>,
) -> &'static str {
    if reject_reason.is_some() {
        return "reject";
    }
    if accept_blocked {
        return "review";
    }
    let duration_exact = duration_delta.is_none_or(|delta| delta <= 3);
    if matches!(
        cand.source_kind,
        CandidateSourceKind::YtmAlbumTrack | CandidateSourceKind::YtmCatalogSong
    ) && title >= 0.98
        && artist >= 0.92
        && duration_exact
    {
        return "exact";
    }
    if matches!(
        cand.source_kind,
        CandidateSourceKind::YtmAlbumTrack | CandidateSourceKind::YtmCatalogSong
    ) && total >= 0.86
        && title >= 0.90
        && artist >= 0.80
    {
        return "strong";
    }
    if matches!(
        cand.source_kind,
        CandidateSourceKind::YtmCatalogVideo | CandidateSourceKind::YoutubeVideoSearch
    ) && official_signal_score(cand) >= 0.7
        && total >= 0.88
    {
        return "strong";
    }
    "review"
}
