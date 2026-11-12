//! PO-token provider detection for `ytt doctor`.
//!
//! PO-token providers are yt-dlp plugins, not executables, so a PATH lookup cannot see them.
//! yt-dlp names the plugin directories it found in its `-v` debug header, and prints that
//! header without a URL and without touching the network.

use std::time::Duration;

use crate::i18n::Language;

const PLUGIN_DIRS_PREFIX: &str = "[debug] Plugin directories:";
const PROVIDER_NAME_HINTS: &[&str] = &["bgutil", "getpot", "pot-provider", "pot_provider"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum PluginDirs {
    /// The probe failed, or this yt-dlp predates the header line.
    Unknown,
    /// `--no-plugin-dirs` or `YTDLP_NO_PLUGINS`.
    Disabled,
    None,
    /// A directory named like a known PO-token provider.
    Provider(String),
    /// Plugins exist, but a directory name cannot tell whether one is a provider.
    Other(usize),
}

pub(super) fn parse_plugin_dirs(debug_header: &str) -> PluginDirs {
    let Some(value) = debug_header
        .lines()
        .rev()
        .find_map(|line| line.trim().strip_prefix(PLUGIN_DIRS_PREFIX))
        .map(str::trim)
    else {
        return PluginDirs::Unknown;
    };
    match value {
        "" => PluginDirs::Unknown,
        "none" => PluginDirs::None,
        "none (disabled)" => PluginDirs::Disabled,
        dirs => {
            let dirs: Vec<&str> = dirs.split(", ").collect();
            dirs.iter()
                .find(|dir| {
                    let dir = dir.to_ascii_lowercase();
                    PROVIDER_NAME_HINTS.iter().any(|hint| dir.contains(hint))
                })
                .map_or(PluginDirs::Other(dirs.len()), |dir| {
                    PluginDirs::Provider((*dir).to_owned())
                })
        }
    }
}

/// Ask the selected yt-dlp, built the way app-owned calls build it. Those calls ignore the
/// user's yt-dlp config, while mpv's ytdl_hook reads it, so a `--plugin-dirs` or
/// `--no-plugin-dirs` set there applies to playback and is not reflected here.
pub(super) fn probe_plugin_dirs() -> PluginDirs {
    let Some(selection) = crate::tools::ytdlp_selection() else {
        return PluginDirs::Unknown;
    };
    let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    else {
        return PluginDirs::Unknown;
    };
    let mut cmd = crate::tools::ytdlp_command_for(&selection.path.to_string_lossy());
    cmd.arg("-v");
    // Without a URL yt-dlp prints the header and exits with a usage error, so the exit status
    // carries no information here.
    runtime
        .block_on(crate::util::process::tokio_output_limited(
            cmd,
            crate::util::process::ProcessProfile::YtDlp,
            Duration::from_secs(10),
            4096,
        ))
        .map_or(PluginDirs::Unknown, |out| {
            parse_plugin_dirs(&String::from_utf8_lossy(&out.stderr_tail))
        })
}

pub(super) fn provider_line(dirs: &PluginDirs, lang: Language) -> String {
    match dirs {
        PluginDirs::Provider(dir) => match lang {
            Language::Korean => format!("✓ PO 토큰 공급자 플러그인 — yt-dlp가 {dir} 을(를) 불러옵니다"),
            Language::Japanese => {
                format!("✓ POトークンプロバイダープラグイン — yt-dlp が {dir} を読み込みます")
            }
            _ => format!("✓ PO-token provider plugin — yt-dlp loads {dir}"),
        },
        PluginDirs::Other(count) => match lang {
            Language::Korean => format!(
                "— yt-dlp 플러그인 디렉터리 {count}개 — 이름으로는 PO 토큰 공급자인지 알 수 없습니다. `yt-dlp -v <url>`의 `PO Token Providers:` 줄에서 확인하세요"
            ),
            Language::Japanese => format!(
                "— yt-dlp プラグインディレクトリ {count} 件 — 名前からは PO トークンプロバイダーか判別できません。`yt-dlp -v <url>` の `PO Token Providers:` 行で確認してください"
            ),
            _ => format!(
                "— yt-dlp plugin directories: {count} — none is named like a PO-token provider; `yt-dlp -v <url>` lists the active ones on its `PO Token Providers:` line"
            ),
        },
        PluginDirs::None => match lang {
            Language::Korean => {
                "✗ yt-dlp 플러그인 없음 — YouTube가 스트림을 차단할 때(403/429) PO 토큰 공급자 플러그인이 도움이 됩니다: https://github.com/yt-dlp/yt-dlp/wiki/PO-Token-Guide"
            }
            Language::Japanese => {
                "✗ yt-dlp プラグインなし — YouTube がストリームを拒否するとき (403/429) は PO トークンプロバイダープラグインが役立ちます: https://github.com/yt-dlp/yt-dlp/wiki/PO-Token-Guide"
            }
            _ => {
                "✗ no yt-dlp plugins — a PO-token provider plugin helps when YouTube blocks streams (403/429): https://github.com/yt-dlp/yt-dlp/wiki/PO-Token-Guide"
            }
        }
        .to_owned(),
        PluginDirs::Disabled => match lang {
            Language::Korean => {
                "✗ yt-dlp 플러그인이 꺼져 있음 (`--no-plugin-dirs` 또는 YTDLP_NO_PLUGINS) — PO 토큰 공급자를 불러올 수 없습니다"
            }
            Language::Japanese => {
                "✗ yt-dlp プラグインが無効 (`--no-plugin-dirs` または YTDLP_NO_PLUGINS) — PO トークンプロバイダーを読み込めません"
            }
            _ => {
                "✗ yt-dlp plugins are disabled (`--no-plugin-dirs` or YTDLP_NO_PLUGINS) — a PO-token provider cannot load"
            }
        }
        .to_owned(),
        PluginDirs::Unknown => match lang {
            Language::Korean => {
                "— yt-dlp 플러그인 디렉터리를 읽지 못했습니다 — `yt-dlp -v`로 확인하세요"
            }
            Language::Japanese => {
                "— yt-dlp のプラグインディレクトリを取得できませんでした — `yt-dlp -v` で確認してください"
            }
            _ => "— could not read yt-dlp's plugin directories — check with `yt-dlp -v`",
        }
        .to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(value: &str) -> String {
        format!(
            "[debug] JS runtimes: deno-2.9.7\n[debug] Plugin directories: {value}\n\nUsage: yt-dlp [OPTIONS] URL [URL...]\n"
        )
    }

    #[test]
    fn a_header_without_the_line_is_unknown() {
        assert_eq!(parse_plugin_dirs(""), PluginDirs::Unknown);
        assert_eq!(
            parse_plugin_dirs("[debug] JS runtimes: none\nUsage: yt-dlp"),
            PluginDirs::Unknown
        );
    }

    #[test]
    fn none_and_disabled_are_distinct() {
        assert_eq!(parse_plugin_dirs(&header("none")), PluginDirs::None);
        assert_eq!(
            parse_plugin_dirs(&header("none (disabled)")),
            PluginDirs::Disabled
        );
    }

    #[test]
    fn a_provider_directory_is_recognized_by_name() {
        let dir = "/home/u/.config/yt-dlp/plugins/bgutil-ytdlp-pot-provider/yt_dlp_plugins";
        assert_eq!(
            parse_plugin_dirs(&header(&format!("/opt/other/yt_dlp_plugins, {dir}"))),
            PluginDirs::Provider(dir.to_owned())
        );
        let zip = "/home/u/yt-dlp-plugins/yt-dlp-getpot-wpc.zip/yt_dlp_plugins";
        assert_eq!(
            parse_plugin_dirs(&header(zip)),
            PluginDirs::Provider(zip.to_owned())
        );
    }

    #[test]
    fn unrelated_names_do_not_count_as_a_provider() {
        assert_eq!(
            parse_plugin_dirs(&header(
                "/home/u/.config/yt-dlp/plugins/spotify-helper/yt_dlp_plugins, /usr/lib/python3/site-packages/yt_dlp_plugins"
            )),
            PluginDirs::Other(2)
        );
    }

    #[test]
    fn every_state_has_a_line_in_every_language() {
        let states = [
            PluginDirs::Unknown,
            PluginDirs::Disabled,
            PluginDirs::None,
            PluginDirs::Provider("/p".to_owned()),
            PluginDirs::Other(3),
        ];
        for state in &states {
            for lang in [Language::English, Language::Korean, Language::Japanese] {
                assert!(!provider_line(state, lang).is_empty());
            }
        }
    }
}
