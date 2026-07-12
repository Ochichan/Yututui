use std::path::PathBuf;

#[derive(Debug, PartialEq, Eq)]
pub(super) struct TerminalRuntimePaths {
    /// Read-only consumers (for example external-tool cookie lookup) may still inspect data.
    pub(super) data_dir: Option<PathBuf>,
    /// Every field below is a mutation destination and is absent for observational instances.
    pub(super) writable_data_dir: Option<PathBuf>,
    pub(super) writable_cache_dir: Option<PathBuf>,
    pub(super) recorder_temp_dir: Option<PathBuf>,
}

pub(super) fn resolve(persistence_read_only: bool) -> TerminalRuntimePaths {
    let data_dir = crate::paths::data_dir();
    let cache_dir = crate::paths::cache_dir();
    let (writable_data_dir, writable_cache_dir) = if persistence_read_only {
        (None, None)
    } else {
        (data_dir.clone(), cache_dir)
    };
    let recorder_temp_dir = writable_cache_dir
        .as_ref()
        .map(|cache_dir| cache_dir.join("recordings"));
    TerminalRuntimePaths {
        data_dir,
        writable_data_dir,
        writable_cache_dir,
        recorder_temp_dir,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overrides_are_used_and_read_only_owners_receive_no_write_root() {
        let base =
            std::env::temp_dir().join(format!("yututui-runtime-paths-{}", std::process::id()));
        let data = base.join("data-override");
        let cache = base.join("cache-override");
        let data_env = data.to_string_lossy().into_owned();
        let cache_env = cache.to_string_lossy().into_owned();

        crate::test_util::env::with_vars(
            &[
                ("YTM_DATA_DIR", Some(data_env.as_str())),
                ("YTM_CACHE_DIR", Some(cache_env.as_str())),
            ],
            || {
                assert_eq!(
                    resolve(false),
                    TerminalRuntimePaths {
                        data_dir: Some(data.clone()),
                        writable_data_dir: Some(data.clone()),
                        writable_cache_dir: Some(cache.clone()),
                        recorder_temp_dir: Some(cache.join("recordings")),
                    }
                );
                assert_eq!(
                    resolve(true),
                    TerminalRuntimePaths {
                        data_dir: Some(data.clone()),
                        writable_data_dir: None,
                        writable_cache_dir: None,
                        recorder_temp_dir: None,
                    },
                    "an observational instance may read data but cannot receive a write path"
                );
            },
        );
    }
}
