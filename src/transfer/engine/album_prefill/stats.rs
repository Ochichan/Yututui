use crate::transfer::checkpoint::MatchStats;
use crate::transfer::matching::{MatchOutcome, MatchScoreBreakdown, YtmMatchDiagnostics};

pub(in crate::transfer::engine) fn merge_ytm_diagnostics(
    stats: &mut MatchStats,
    diagnostics: YtmMatchDiagnostics,
) {
    stats.catalog_searches += diagnostics.catalog_searches;
    stats.video_searches += diagnostics.video_searches;
    stats.ytm_video_catalog_searches += diagnostics.ytm_video_searches;
    stats.public_video_searches += diagnostics.public_video_searches;
    stats.retries = stats
        .retries
        .saturating_add(diagnostics.public_video_retries)
        .saturating_add(diagnostics.preflight_retries);
    stats.preflight_lookups += diagnostics.preflight_lookups;
    stats.catalog_http_pages += diagnostics.catalog_searches;
    stats.video_process_spawns += diagnostics.public_video_searches;
    stats.preflight_process_spawns += diagnostics.preflight_lookups;
    if diagnostics.preflight_failures > 0 {
        let failures = stats
            .provider_errors
            .entry("metadata_preflight_unavailable".to_owned())
            .or_default();
        *failures = failures.saturating_add(diagnostics.preflight_failures);
    }
    stats.authenticated_catalog_degraded += diagnostics.authenticated_catalog_degraded;
    stats.query_cache_hits += diagnostics.query_cache_hits;
    stats.video_meta_cache_hits += diagnostics.video_meta_cache_hits;
}

pub(in crate::transfer::engine) fn record_match_outcome_stats(
    stats: &mut MatchStats,
    outcome: &MatchOutcome,
) {
    match outcome {
        MatchOutcome::Matched {
            score_breakdown: Some(score),
            ..
        } => record_score_stats(stats, score),
        MatchOutcome::Ambiguous { candidates } => {
            for candidate in candidates {
                if let Some(score) = &candidate.score_breakdown {
                    record_score_stats(stats, score);
                }
            }
        }
        _ => {}
    }
}

fn record_score_stats(stats: &mut MatchStats, score: &MatchScoreBreakdown) {
    stats.bump_source_kind(&score.source_kind);
    stats.bump_quality_tier(&score.quality_tier);
    for code in &score.reason_codes {
        stats.bump_reason_code(code);
    }
    if let Some(reason) = &score.reject_reason {
        stats.bump_reason_code(reason);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_diagnostics_preserve_process_attempts_and_retries() {
        let mut stats = MatchStats::default();
        merge_ytm_diagnostics(
            &mut stats,
            YtmMatchDiagnostics {
                video_searches: 3,
                public_video_searches: 2,
                public_video_retries: 1,
                preflight_lookups: 3,
                preflight_retries: 1,
                preflight_failures: 1,
                ..YtmMatchDiagnostics::default()
            },
        );

        assert_eq!(stats.video_searches, 3);
        assert_eq!(stats.public_video_searches, 2);
        assert_eq!(stats.video_process_spawns, 2);
        assert_eq!(stats.retries, 2);
        assert_eq!(stats.preflight_process_spawns, 3);
        assert_eq!(
            stats.provider_errors.get("metadata_preflight_unavailable"),
            Some(&1)
        );
    }
}
