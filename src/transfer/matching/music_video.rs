use crate::api::ytmusic::YtmMusicVideoType;
use crate::streaming::musicgate;

use super::quality::official_signal_score;
use super::{CandidateSourceKind, IdentityGate, MatchCandidate};

pub(super) fn is_official_video_presentation(cand: &MatchCandidate) -> bool {
    matches!(
        cand.source_kind,
        CandidateSourceKind::YtmCatalogVideo | CandidateSourceKind::YoutubeVideoSearch
    ) && official_signal_score(cand) >= 0.7
        && (cand.music_video_type == Some(YtmMusicVideoType::Omv)
            || candidate_has_music_video_marker(cand))
}

fn candidate_has_music_video_marker(cand: &MatchCandidate) -> bool {
    cand.metadata_title
        .as_deref()
        .into_iter()
        .chain(std::iter::once(cand.title.as_str()))
        .any(|title| {
            let title = title.to_lowercase();
            [
                "official music video",
                "official video",
                "official mv",
                "music video",
                " m v",
                " mv",
                "뮤직비디오",
            ]
            .iter()
            .any(|marker| title.contains(marker))
        })
}

pub(super) fn apply_music_video_gate(
    gate: &mut IdentityGate,
    cand: &MatchCandidate,
    title_score: f32,
    artist_score: f32,
) {
    if !matches!(
        cand.source_kind,
        CandidateSourceKind::YtmCatalogVideo
            | CandidateSourceKind::YoutubeVideoSearch
            | CandidateSourceKind::Unknown
    ) {
        gate.hard_reject.get_or_insert("music_video_required");
        gate.reasons.push("music_video_required");
        return;
    }

    let video_type = cand.music_video_type.unwrap_or(YtmMusicVideoType::Unknown);
    match video_type {
        YtmMusicVideoType::Ugc => reject_type(gate, "music_video_type_ugc"),
        YtmMusicVideoType::Atv => reject_type(gate, "music_video_type_atv"),
        YtmMusicVideoType::Shoulder => reject_type(gate, "music_video_type_shoulder"),
        YtmMusicVideoType::Upload => reject_type(gate, "music_video_type_upload"),
        YtmMusicVideoType::Episode => reject_type(gate, "music_video_type_episode"),
        YtmMusicVideoType::Omv | YtmMusicVideoType::OfficialSourceMusic => {
            gate.reasons.push(match video_type {
                YtmMusicVideoType::Omv => "music_video_type_omv",
                _ => "music_video_type_official_source_music",
            });
            if !cand.preflighted {
                gate.accept_blocked = true;
                gate.reasons.push("music_video_preflight_required");
            }
        }
        YtmMusicVideoType::Unknown => {
            gate.reasons.push("music_video_type_unknown");
            let channel = cand.channel.as_deref().unwrap_or(&cand.artist);
            let has_video_marker = candidate_has_music_video_marker(cand);
            let verified_corroboration = cand.channel_verified == Some(true)
                && has_video_marker
                && title_score >= 0.88
                && artist_score >= 0.75;
            if cand.preflighted
                && has_video_marker
                && (musicgate::is_trusted_music_channel(channel) || verified_corroboration)
            {
                gate.reasons.push("music_video_official_corroborated");
            } else {
                gate.accept_blocked = true;
                gate.reasons.push("music_video_review_only");
            }
        }
    }
}

fn reject_type(gate: &mut IdentityGate, reason: &'static str) {
    gate.hard_reject.get_or_insert(reason);
    gate.reasons.push("music_video_hard_reject");
    gate.reasons.push(reason);
}
