//! Spawn-time capability, override, and private-root handling for long-form packet caching.

use std::io;
use std::path::{Component, Path, PathBuf};

use crate::config::MpvAudioRuntimeConfig;

use super::long_form_seek::{CacheOptionFamily, CacheReason, ControllerCapability};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OverrideSource {
    Config,
    Environment,
}

impl OverrideSource {
    pub const fn id(self) -> &'static str {
        match self {
            Self::Config => "config",
            Self::Environment => "environment",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CacheOverride {
    pub source: OverrideSource,
    pub cache_activation: bool,
    pub cache_disabled: bool,
    pub seekable_cache_disabled: bool,
    pub cache_directory: bool,
    pub unlink_lifecycle: bool,
    pub cache_pause_wait: bool,
}

impl CacheOverride {
    pub const fn controls_managed_cache(self) -> bool {
        self.cache_activation
            || self.cache_disabled
            || self.seekable_cache_disabled
            || self.cache_directory
            || self.unlink_lifecycle
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CacheSpawnSupport {
    pub capability: ControllerCapability,
    pub option_family: Option<CacheOptionFamily>,
    pub override_source: Option<OverrideSource>,
    pub cache_dir: Option<PathBuf>,
    pub spawn_args: Vec<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CacheRootAvailability {
    Available,
    ReadOnly,
    Unavailable,
}

impl CacheRootAvailability {
    pub const fn id(self) -> &'static str {
        match self {
            Self::Available => "available",
            Self::ReadOnly => "read_only",
            Self::Unavailable => "unavailable",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StaticCacheSupport {
    pub capability: ControllerCapability,
    pub option_family: Option<CacheOptionFamily>,
    pub override_source: Option<OverrideSource>,
    pub cache_root: CacheRootAvailability,
}

impl CacheSpawnSupport {
    fn unavailable(reason: CacheReason) -> Self {
        Self {
            capability: ControllerCapability::Unavailable(reason),
            option_family: None,
            override_source: None,
            cache_dir: None,
            spawn_args: Vec::new(),
        }
    }

    fn overridden(source: OverrideSource) -> Self {
        Self {
            capability: ControllerCapability::Overridden,
            option_family: None,
            override_source: Some(source),
            cache_dir: None,
            spawn_args: Vec::new(),
        }
    }
}

/// Detect whether user-controlled raw argv owns a managed cache setting.
pub fn detect_cache_override(
    config_args: &[String],
    environment_args: Option<&str>,
) -> Option<CacheOverride> {
    // Environment argv is appended last, so identify it as the controlling source when both
    // layers mention lifecycle options.
    let environment = split_shell_like(environment_args.unwrap_or_default());
    inspect_args(
        environment.iter().map(String::as_str),
        OverrideSource::Environment,
    )
    .filter(|override_| override_.controls_managed_cache())
    .or_else(|| {
        inspect_args(
            config_args.iter().map(String::as_str),
            OverrideSource::Config,
        )
        .filter(|override_| override_.controls_managed_cache())
    })
}

/// Probe option names without parsing mpv's version and prepare an owner-private cache root.
///
/// `probe` must use the selected mpv executable identity as part of its cache key. Unknown flags
/// are never forwarded to the real player process.
pub fn prepare_cache_support(
    audio: &MpvAudioRuntimeConfig,
    writable_cache_root: Option<&Path>,
    environment_args: Option<&str>,
    mut probe: impl FnMut(&str) -> bool,
) -> CacheSpawnSupport {
    if let Some(override_) = detect_cache_override(&audio.extra_args, environment_args) {
        return CacheSpawnSupport::overridden(override_.source);
    }
    let Some(root) = writable_cache_root else {
        return CacheSpawnSupport::unavailable(CacheReason::ReadOnlyInstance);
    };
    let Some(family) = probe_option_family(&mut probe) else {
        return CacheSpawnSupport::unavailable(CacheReason::UnsupportedMpv);
    };
    let cache_dir = root.join("mpv-current-media");
    if prepare_private_cache_dir(root, &cache_dir).is_err() {
        return CacheSpawnSupport::unavailable(CacheReason::CacheRootUnavailable);
    }
    let path = cache_dir.to_string_lossy();
    let mut spawn_args = vec!["--cache-on-disk=no".to_owned()];
    match family {
        CacheOptionFamily::Modern => {
            spawn_args.push(format!("--demuxer-cache-dir={path}"));
            spawn_args.push("--demuxer-cache-unlink-files=immediate".to_owned());
        }
        CacheOptionFamily::Legacy => {
            spawn_args.push(format!("--cache-dir={path}"));
            spawn_args.push("--cache-unlink-files=immediate".to_owned());
        }
    }
    CacheSpawnSupport {
        capability: ControllerCapability::Available(family),
        option_family: Some(family),
        override_source: None,
        cache_dir: Some(cache_dir),
        spawn_args,
    }
}

/// Provision the owner-private cache namespace without accepting a symlink/reparse component.
///
/// `ensure_private_dir` protects its final leaf. The packet-cache path additionally treats every
/// existing ancestor as part of the lifecycle boundary because mpv later opens the directory by
/// pathname. Checking before and after each creation prevents an already-unsafe tree from being
/// accepted and detects a replacement race before the path is handed to mpv. Once `root` exists,
/// its private permissions make its child namespace owner-only.
fn prepare_private_cache_dir(root: &Path, cache_dir: &Path) -> io::Result<()> {
    validate_real_directory_components(root)?;
    crate::util::safe_fs::ensure_private_dir(root)?;
    validate_real_directory_components(root)?;

    validate_real_directory_components(cache_dir)?;
    crate::util::safe_fs::ensure_private_dir(cache_dir)?;
    validate_real_directory_components(cache_dir)
}

fn validate_real_directory_components(path: &Path) -> io::Result<()> {
    if !path.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "managed cache root must be absolute",
        ));
    }
    let mut current = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => {
                current.push(prefix.as_os_str());
                // On Windows a canonical verbatim prefix (`\\?\C:`) reports itself as rooted,
                // but it is not an openable path until the following RootDir is appended.
                continue;
            }
            Component::RootDir | Component::Normal(_) => {
                current.push(component.as_os_str());
            }
            Component::CurDir | Component::ParentDir => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "managed cache root must be lexically normalized",
                ));
            }
        }
        // Relative paths were rejected above; this guard only covers platform component quirks.
        if !current.has_root() {
            continue;
        }
        match std::fs::symlink_metadata(&current) {
            Ok(metadata) if path_metadata_is_link(&metadata) || !metadata.is_dir() => {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "managed cache path contains a link or non-directory component",
                ));
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => break,
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

/// Read-only capability/root inspection for doctor. No directory or probe file is created.
pub fn inspect_cache_support(
    config_args: &[String],
    writable_cache_root: Option<&Path>,
    environment_args: Option<&str>,
    mut probe: impl FnMut(&str) -> bool,
) -> StaticCacheSupport {
    let cache_root = inspect_cache_root(writable_cache_root);
    if let Some(override_) = detect_cache_override(config_args, environment_args) {
        return StaticCacheSupport {
            capability: ControllerCapability::Overridden,
            option_family: None,
            override_source: Some(override_.source),
            cache_root,
        };
    }
    let option_family = probe_option_family(&mut probe);
    let capability = match (cache_root, option_family) {
        (CacheRootAvailability::Available, Some(family)) => ControllerCapability::Available(family),
        (CacheRootAvailability::ReadOnly, _) => {
            ControllerCapability::Unavailable(CacheReason::ReadOnlyInstance)
        }
        (CacheRootAvailability::Unavailable, _) => {
            ControllerCapability::Unavailable(CacheReason::CacheRootUnavailable)
        }
        (CacheRootAvailability::Available, None) => {
            ControllerCapability::Unavailable(CacheReason::UnsupportedMpv)
        }
    };
    StaticCacheSupport {
        capability,
        option_family,
        override_source: None,
        cache_root,
    }
}

fn probe_option_family(probe: &mut impl FnMut(&str) -> bool) -> Option<CacheOptionFamily> {
    let family =
        if probe("--demuxer-cache-dir=.") && probe("--demuxer-cache-unlink-files=immediate") {
            Some(CacheOptionFamily::Modern)
        } else if probe("--cache-dir=.") && probe("--cache-unlink-files=immediate") {
            Some(CacheOptionFamily::Legacy)
        } else {
            None
        }?;
    probe("--cache-on-disk=no").then_some(family)
}

fn inspect_cache_root(root: Option<&Path>) -> CacheRootAvailability {
    let Some(root) = root else {
        return CacheRootAvailability::Unavailable;
    };
    if validate_real_directory_components(root).is_err() {
        return CacheRootAvailability::Unavailable;
    }
    for candidate in root.ancestors() {
        match std::fs::symlink_metadata(candidate) {
            Ok(metadata) => {
                if path_metadata_is_link(&metadata) || !metadata.is_dir() {
                    return CacheRootAvailability::Unavailable;
                }
                return if metadata_allows_writes(&metadata) {
                    CacheRootAvailability::Available
                } else {
                    CacheRootAvailability::ReadOnly
                };
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => return CacheRootAvailability::Unavailable,
        }
    }
    CacheRootAvailability::Unavailable
}

#[cfg(unix)]
fn metadata_allows_writes(metadata: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    metadata.permissions().mode() & 0o222 != 0
}

#[cfg(not(unix))]
fn metadata_allows_writes(metadata: &std::fs::Metadata) -> bool {
    !metadata.permissions().readonly()
}

#[cfg(windows)]
fn path_metadata_is_link(metadata: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
fn path_metadata_is_link(metadata: &std::fs::Metadata) -> bool {
    metadata.file_type().is_symlink()
}

fn inspect_args<'a>(
    args: impl Iterator<Item = &'a str>,
    source: OverrideSource,
) -> Option<CacheOverride> {
    let mut result = CacheOverride {
        source,
        cache_activation: false,
        cache_disabled: false,
        seekable_cache_disabled: false,
        cache_directory: false,
        unlink_lifecycle: false,
        cache_pause_wait: false,
    };
    let mut saw_relevant = false;
    for raw in args {
        let Some((name, value)) = normalize_option(raw) else {
            continue;
        };
        match name.as_str() {
            "cache-on-disk" => {
                result.cache_activation = true;
                saw_relevant = true;
            }
            "cache" if is_false(value.as_deref()) => {
                result.cache_disabled = true;
                saw_relevant = true;
            }
            "demuxer-seekable-cache" if is_false(value.as_deref()) => {
                result.seekable_cache_disabled = true;
                saw_relevant = true;
            }
            "demuxer-cache-dir" | "cache-dir" => {
                result.cache_directory = true;
                saw_relevant = true;
            }
            "demuxer-cache-unlink-files" | "cache-unlink-files" => {
                result.unlink_lifecycle = true;
                saw_relevant = true;
            }
            "cache-pause-wait" => {
                result.cache_pause_wait = true;
                saw_relevant = true;
            }
            _ => {}
        }
    }
    saw_relevant.then_some(result)
}

fn normalize_option(raw: &str) -> Option<(String, Option<String>)> {
    let mut option = raw.trim().strip_prefix('-')?.trim_start_matches('-');
    if option.is_empty() {
        return None;
    }
    let mut negated = false;
    if let Some(rest) = option.strip_prefix("no-") {
        option = rest;
        negated = true;
    }
    let (name, value) = option
        .split_once('=')
        .map_or((option, None), |(name, value)| (name, Some(value)));
    let name = name.replace('_', "-").to_ascii_lowercase();
    let value = if negated {
        Some("no".to_owned())
    } else {
        value.map(|value| value.trim().to_ascii_lowercase())
    };
    Some((name, value))
}

fn is_false(value: Option<&str>) -> bool {
    matches!(value, Some("no" | "false" | "0"))
}

/// Quote-aware argv splitting for the environment escape hatch. No expansion is performed.
fn split_shell_like(value: &str) -> Vec<String> {
    let mut args = Vec::new();
    let mut current = String::new();
    let mut quote = None;
    let mut active = false;
    for character in value.chars() {
        match quote {
            Some(expected) if character == expected => quote = None,
            Some(_) => current.push(character),
            None if character == '\'' || character == '"' => {
                quote = Some(character);
                active = true;
            }
            None if character.is_whitespace() => {
                if active {
                    args.push(std::mem::take(&mut current));
                    active = false;
                }
            }
            None => {
                current.push(character);
                active = true;
            }
        }
    }
    if active || quote.is_some() {
        args.push(current);
    }
    args
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::LongFormSeekOptimization;

    fn audio(extra_args: &[&str]) -> MpvAudioRuntimeConfig {
        MpvAudioRuntimeConfig {
            output: None,
            device: None,
            cache_forward: "32MiB".to_owned(),
            cache_back: "8MiB".to_owned(),
            long_form_seek_optimization: LongFormSeekOptimization::Auto,
            extra_args: extra_args.iter().map(|value| (*value).to_owned()).collect(),
        }
    }

    fn test_scratch(label: &str) -> PathBuf {
        let target = std::env::current_dir()
            .expect("test working directory")
            .join("target")
            .join("cache-support-tests");
        std::fs::create_dir_all(&target).expect("create cache-support test scratch");
        std::fs::canonicalize(target)
            .expect("canonical cache-support test scratch")
            .join(format!("{label}-{}", std::process::id()))
    }

    #[test]
    fn detects_all_lifecycle_overrides_and_precedence() {
        for arg in [
            "--cache-on-disk=yes",
            "--no-cache",
            "--cache=no",
            "--no-demuxer-seekable-cache",
            "--demuxer-cache-dir=/tmp/x",
            "--cache_dir=/tmp/x",
            "--cache-unlink-files=no",
        ] {
            let found = detect_cache_override(&[arg.to_owned()], None).unwrap();
            assert_eq!(found.source, OverrideSource::Config, "{arg}");
            assert!(found.controls_managed_cache(), "{arg}");
        }
        let found = detect_cache_override(
            &["--cache-on-disk=no".to_owned()],
            Some("--cache-on-disk=yes"),
        )
        .unwrap();
        assert_eq!(found.source, OverrideSource::Environment);
    }

    #[test]
    fn pause_wait_alone_is_not_a_disk_lifecycle_override() {
        assert!(detect_cache_override(&["--cache-pause-wait=0.25".to_owned()], None).is_none());
    }

    #[test]
    fn environment_parser_preserves_quoted_paths() {
        let found = detect_cache_override(
            &[],
            Some(r#"--ao=null --demuxer-cache-dir="/private/path with spaces""#),
        )
        .unwrap();
        assert_eq!(found.source, OverrideSource::Environment);
        assert!(found.cache_directory);
    }

    #[test]
    fn missing_writable_root_is_read_only_without_probe() {
        let mut probes = 0;
        let support = prepare_cache_support(&audio(&[]), None, None, |_| {
            probes += 1;
            true
        });
        assert_eq!(
            support.capability,
            ControllerCapability::Unavailable(CacheReason::ReadOnlyInstance)
        );
        assert_eq!(probes, 0);
    }

    #[test]
    fn override_never_creates_or_probes() {
        let root = std::env::temp_dir().join(format!(
            "yututui-cache-support-override-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let mut probes = 0;
        let support =
            prepare_cache_support(&audio(&["--cache-on-disk=no"]), Some(&root), None, |_| {
                probes += 1;
                true
            });
        assert_eq!(support.capability, ControllerCapability::Overridden);
        assert_eq!(probes, 0);
        assert!(!root.exists());
    }

    #[test]
    fn modern_then_legacy_probe_selection_is_version_free() {
        let root = test_scratch("family");
        let _ = std::fs::remove_dir_all(&root);
        crate::util::safe_fs::ensure_private_dir(&root).unwrap();

        let modern = prepare_cache_support(&audio(&[]), Some(&root), None, |_| true);
        assert_eq!(modern.option_family, Some(CacheOptionFamily::Modern));
        assert!(
            modern
                .spawn_args
                .iter()
                .any(|arg| arg.starts_with("--demuxer-cache-dir="))
        );
        assert!(
            modern
                .spawn_args
                .iter()
                .any(|arg| arg == "--demuxer-cache-unlink-files=immediate")
        );

        let legacy = prepare_cache_support(&audio(&[]), Some(&root), None, |flag| {
            !flag.starts_with("--demuxer-cache-")
        });
        assert_eq!(legacy.option_family, Some(CacheOptionFamily::Legacy));
        assert!(
            legacy
                .spawn_args
                .iter()
                .any(|arg| arg.starts_with("--cache-dir="))
        );
        assert!(
            legacy
                .spawn_args
                .iter()
                .any(|arg| arg == "--cache-unlink-files=immediate")
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn static_inspection_is_read_only_and_reports_the_probed_family() {
        let base = test_scratch("inspect");
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let missing = base.join("not-created");
        let support = inspect_cache_support(&[], Some(&missing), None, |flag| {
            !flag.starts_with("--demuxer-cache-")
        });
        assert_eq!(support.option_family, Some(CacheOptionFamily::Legacy));
        assert_eq!(support.cache_root, CacheRootAvailability::Available);
        assert_eq!(
            support.capability,
            ControllerCapability::Available(CacheOptionFamily::Legacy)
        );
        assert!(!missing.exists(), "static inspection provisioned the root");
        let _ = std::fs::remove_dir_all(base);
    }

    #[cfg(unix)]
    #[test]
    fn static_inspection_rejects_symlink_and_read_only_roots() {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let base = std::fs::canonicalize(std::env::temp_dir())
            .unwrap()
            .join(format!(
                "yututui-cache-support-inspect-perms-{}",
                std::process::id()
            ));
        let _ = std::fs::remove_dir_all(&base);
        let real = base.join("real");
        let link = base.join("link");
        std::fs::create_dir_all(&real).unwrap();
        symlink(&real, &link).unwrap();
        assert_eq!(
            inspect_cache_root(Some(&link)),
            CacheRootAvailability::Unavailable
        );

        let original = std::fs::metadata(&real).unwrap().permissions();
        std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o500)).unwrap();
        assert_eq!(
            inspect_cache_root(Some(&real)),
            CacheRootAvailability::ReadOnly
        );
        std::fs::set_permissions(&real, original).unwrap();
        let _ = std::fs::remove_dir_all(base);
    }

    #[cfg(unix)]
    #[test]
    fn provisioning_rejects_symlink_roots_and_ancestors_without_touching_targets() {
        use std::os::unix::fs::symlink;

        let base = std::fs::canonicalize(std::env::temp_dir())
            .unwrap()
            .join(format!(
                "yututui-cache-support-provision-links-{}",
                std::process::id()
            ));
        let _ = std::fs::remove_dir_all(&base);
        let target = base.join("target");
        let root_link = base.join("root-link");
        let ancestor_link = base.join("ancestor-link");
        std::fs::create_dir_all(&target).unwrap();
        symlink(&target, &root_link).unwrap();
        symlink(&target, &ancestor_link).unwrap();

        for unsafe_root in [root_link, ancestor_link.join("nested")] {
            let support = prepare_cache_support(&audio(&[]), Some(&unsafe_root), None, |_| true);
            assert_eq!(
                support.capability,
                ControllerCapability::Unavailable(CacheReason::CacheRootUnavailable)
            );
        }
        assert!(!target.join("mpv-current-media").exists());
        assert!(!target.join("nested").exists());
        let _ = std::fs::remove_dir_all(base);
    }

    #[cfg(unix)]
    #[test]
    fn provisioning_rejects_a_replaced_cache_leaf_before_reuse() {
        use std::os::unix::fs::symlink;

        let base = std::fs::canonicalize(std::env::temp_dir())
            .unwrap()
            .join(format!(
                "yututui-cache-support-replaced-leaf-{}",
                std::process::id()
            ));
        let _ = std::fs::remove_dir_all(&base);
        let root = base.join("root");
        let outside = base.join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        let support = prepare_cache_support(&audio(&[]), Some(&root), None, |_| true);
        assert_eq!(
            support.capability,
            ControllerCapability::Available(CacheOptionFamily::Modern)
        );

        let cache = root.join("mpv-current-media");
        std::fs::remove_dir(&cache).unwrap();
        symlink(&outside, &cache).unwrap();
        let rejected = prepare_cache_support(&audio(&[]), Some(&root), None, |_| true);
        assert_eq!(
            rejected.capability,
            ControllerCapability::Unavailable(CacheReason::CacheRootUnavailable)
        );
        assert!(std::fs::read_dir(&outside).unwrap().next().is_none());
        let _ = std::fs::remove_dir_all(base);
    }
}
