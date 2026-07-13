//! Read-only long-form seek diagnostics. This module never provisions the cache root.

use crate::config;
use crate::player::cache_support::{self, CacheRootAvailability, OverrideSource};
use crate::player::long_form_seek::{
    CACHE_SOFT_TARGET_BYTES, CacheEffectiveState, CacheOptionFamily, CacheReason,
    ControllerCapability, FREE_SPACE_RESERVE_BYTES,
};

#[derive(Debug, PartialEq, Eq)]
struct Summary {
    requested: &'static str,
    effective: CacheEffectiveState,
    reason: CacheReason,
    option_family: &'static str,
    override_source: &'static str,
    cache_root: CacheRootAvailability,
    runtime_owner: bool,
    last_failure: Option<CacheReason>,
    last_cleanup_ms: Option<u64>,
}

pub(super) fn print(audio: &config::MpvAudioConfig, verbose: bool) {
    let mut summary = inspect(audio);
    if let Some(runtime) = inspect_running_owner() {
        apply_runtime(&mut summary, runtime);
    }
    println!("  long-form seek requested: {}", summary.requested);
    println!("  long-form seek effective: {}", summary.effective.id());
    println!("  long-form seek reason: {}", summary.reason.id());
    println!(
        "  long-form seek status source: {}",
        if summary.runtime_owner {
            "running daemon owner"
        } else {
            "offline capability inspection"
        }
    );
    println!("  long-form seek option family: {}", summary.option_family);
    println!(
        "  long-form seek custom override: {}",
        summary.override_source
    );
    println!("  long-form seek cache root: {}", summary.cache_root.id());
    println!(
        "  long-form seek soft target: {} MiB",
        CACHE_SOFT_TARGET_BYTES / (1024 * 1024)
    );
    println!(
        "  long-form seek free-space reserve: max({} GiB, 5%)",
        FREE_SPACE_RESERVE_BYTES / (1024 * 1024 * 1024)
    );
    if verbose {
        match (summary.runtime_owner, summary.last_failure) {
            (_, Some(reason)) => {
                println!("  long-form seek last runtime failure: {}", reason.id())
            }
            (true, None) => println!("  long-form seek last runtime failure: none reported"),
            (false, None) => println!(
                "  long-form seek last runtime failure: unavailable (no compatible daemon owner)"
            ),
        }
        match (summary.runtime_owner, summary.last_cleanup_ms) {
            (_, Some(elapsed_ms)) => {
                println!("  long-form seek last cleanup: completed in {elapsed_ms} ms")
            }
            (true, None) => println!("  long-form seek last cleanup: none reported"),
            (false, None) => {
                println!("  long-form seek last cleanup: unavailable (no compatible daemon owner)")
            }
        }
    }
}

fn inspect_running_owner() -> Option<crate::remote::proto::LongFormSeekRuntimeSnapshot> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .ok()?;
    let response = runtime
        .block_on(crate::remote::client::send(
            crate::remote::proto::RemoteCommand::Status,
        ))
        .ok()?;
    response.status?.settings.long_form_seek
}

fn apply_runtime(
    summary: &mut Summary,
    runtime: crate::remote::proto::LongFormSeekRuntimeSnapshot,
) {
    let effective = serde_json::to_value(runtime.effective)
        .ok()
        .and_then(|value| serde_json::from_value(value).ok());
    let reason = serde_json::to_value(runtime.reason)
        .ok()
        .and_then(|value| serde_json::from_value(value).ok());
    let last_failure = runtime.last_failure.and_then(|failure| {
        serde_json::to_value(failure)
            .ok()
            .and_then(|value| serde_json::from_value(value).ok())
    });
    let (Some(effective), Some(reason)) = (effective, reason) else {
        return;
    };
    summary.effective = effective;
    summary.reason = reason;
    summary.runtime_owner = true;
    summary.last_failure = last_failure;
    summary.last_cleanup_ms = runtime.last_cleanup_ms;
}

fn inspect(audio: &config::MpvAudioConfig) -> Summary {
    let env_extra = std::env::var("YTM_MPV_EXTRA").ok();
    let support = cache_support::inspect_cache_support(
        &audio.extra_args,
        crate::paths::cache_dir().as_deref(),
        env_extra.as_deref(),
        crate::player::mpv::flag_supported,
    );
    summarize(audio, support)
}

fn summarize(
    audio: &config::MpvAudioConfig,
    support: cache_support::StaticCacheSupport,
) -> Summary {
    let (effective, reason) = match support.capability {
        ControllerCapability::Available(_) => (CacheEffectiveState::NoMedia, CacheReason::NoMedia),
        ControllerCapability::Overridden => (
            CacheEffectiveState::Overridden,
            CacheReason::CustomMpvOverride,
        ),
        ControllerCapability::Unavailable(reason) => (CacheEffectiveState::Unavailable, reason),
    };
    Summary {
        requested: audio.long_form_seek_optimization.id(),
        effective,
        reason,
        option_family: match support.option_family {
            Some(CacheOptionFamily::Modern) => "modern",
            Some(CacheOptionFamily::Legacy) => "legacy",
            None => "unsupported",
        },
        override_source: match support.override_source {
            Some(OverrideSource::Config) => "config",
            Some(OverrideSource::Environment) => "environment",
            None => "none",
        },
        cache_root: support.cache_root,
        runtime_owner: false,
        last_failure: None,
        last_cleanup_ms: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summary_uses_real_override_parser_without_retaining_values() {
        let audio = config::MpvAudioConfig {
            extra_args: vec!["--demuxer-cache-dir=/private/signed-token".to_owned()],
            ..config::MpvAudioConfig::default()
        };
        let support = cache_support::inspect_cache_support(
            &audio.extra_args,
            crate::paths::cache_dir().as_deref(),
            None,
            |_| true,
        );
        assert_eq!(support.override_source, Some(OverrideSource::Config));
        assert_eq!(support.capability, ControllerCapability::Overridden);
        let summary = summarize(&audio, support);
        assert_eq!(summary.effective, CacheEffectiveState::Overridden);
        assert_eq!(summary.reason, CacheReason::CustomMpvOverride);
        assert_eq!(summary.override_source, "config");
    }

    #[test]
    fn offline_summary_reports_capability_fallbacks_instead_of_claiming_runtime_state() {
        let audio = config::MpvAudioConfig::default();
        let available = summarize(
            &audio,
            cache_support::StaticCacheSupport {
                capability: ControllerCapability::Available(CacheOptionFamily::Modern),
                option_family: Some(CacheOptionFamily::Modern),
                override_source: None,
                cache_root: CacheRootAvailability::Available,
            },
        );
        assert_eq!(available.effective, CacheEffectiveState::NoMedia);
        assert_eq!(available.reason, CacheReason::NoMedia);

        let unavailable = summarize(
            &audio,
            cache_support::StaticCacheSupport {
                capability: ControllerCapability::Unavailable(CacheReason::ReadOnlyInstance),
                option_family: Some(CacheOptionFamily::Legacy),
                override_source: None,
                cache_root: CacheRootAvailability::ReadOnly,
            },
        );
        assert_eq!(unavailable.effective, CacheEffectiveState::Unavailable);
        assert_eq!(unavailable.reason, CacheReason::ReadOnlyInstance);
        assert_eq!(unavailable.cache_root, CacheRootAvailability::ReadOnly);
    }

    #[test]
    fn compatible_daemon_snapshot_overrides_only_privacy_safe_runtime_fields() {
        let audio = config::MpvAudioConfig::default();
        let mut summary = summarize(
            &audio,
            cache_support::StaticCacheSupport {
                capability: ControllerCapability::Available(CacheOptionFamily::Modern),
                option_family: Some(CacheOptionFamily::Modern),
                override_source: None,
                cache_root: CacheRootAvailability::Available,
            },
        );
        apply_runtime(
            &mut summary,
            crate::remote::proto::LongFormSeekRuntimeSnapshot {
                effective: crate::remote::proto::LongFormSeekEffective::DiskActive,
                reason: crate::remote::proto::LongFormSeekReason::AutoUncachedSeek,
                last_failure: Some(crate::remote::proto::LongFormSeekReason::ProbeFailed),
                last_cleanup_ms: Some(275),
            },
        );
        assert!(summary.runtime_owner);
        assert_eq!(summary.effective, CacheEffectiveState::DiskActive);
        assert_eq!(summary.reason, CacheReason::AutoUncachedSeek);
        assert_eq!(summary.last_failure, Some(CacheReason::ProbeFailed));
        assert_eq!(summary.last_cleanup_ms, Some(275));
        assert_eq!(summary.option_family, "modern");
        assert_eq!(summary.override_source, "none");
    }
}
