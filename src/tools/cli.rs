//! `ytt tools` — manage the app-managed yt-dlp from the command line.
//!
//! `status` prints which yt-dlp/mpv the app would use (same resolution as startup);
//! `update` forces a check-and-install against the configured channel. Both run in
//! the synchronous main path before any terminal setup, like `ytt doctor`.

use crate::{config, i18n, tools};

pub fn run(args: &[String]) -> i32 {
    match args.first().map(String::as_str) {
        None | Some("status") => status(),
        Some("update") => update(),
        Some("--help" | "-h" | "help") => {
            help();
            0
        }
        Some(other) => {
            eprintln!("ytt tools: unknown command `{other}`");
            help();
            2
        }
    }
}

fn help() {
    println!("Usage: ytt tools <command>");
    println!();
    println!("Commands:");
    println!("  status   Show which yt-dlp/mpv the app uses (managed, system, or override)");
    println!("  update   Check the release channel now and install a newer yt-dlp if available");
}

/// A current-thread runtime for the one-shot commands (precedent: the auth/transfer
/// subcommands — never the multi-thread TUI runtime).
fn block_on<F: std::future::Future>(fut: F) -> Option<F::Output> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .ok()
        .map(|rt| rt.block_on(fut))
}

fn status() -> i32 {
    let cfg = config::Config::load();
    i18n::set_language(cfg.effective_language());
    let kr = i18n::is_korean();

    let Some(()) = block_on(tools::init(&cfg.tools)) else {
        eprintln!("ytt tools: failed to build async runtime");
        return 1;
    };

    match tools::ytdlp_selection() {
        Some(sel) => println!(
            "yt-dlp: {} {} · {}",
            sel.source.label(),
            sel.version.as_deref().unwrap_or("?"),
            sel.path.display()
        ),
        None => println!(
            "yt-dlp: {}",
            if kr {
                "없음 — `ytt tools update`로 받으세요"
            } else {
                "none found — fetch one with `ytt tools update`"
            }
        ),
    }

    let state = tools::ytdlp::load_state();
    let channel = state.channel.unwrap_or_else(|| cfg.tools.channel());
    if !cfg.tools.managed_enabled() {
        println!(
            "managed: {}",
            if kr {
                "꺼짐 (tools.ytdlp_managed = false)"
            } else {
                "disabled (tools.ytdlp_managed = false)"
            }
        );
    } else if tools::ytdlp::asset_name().is_none() {
        println!(
            "managed: {}",
            if kr {
                "이 플랫폼은 미지원 (시스템 yt-dlp 사용)"
            } else {
                "unsupported on this platform (system yt-dlp is used)"
            }
        );
    } else {
        match tools::ytdlp::installed_managed_path() {
            Some(path) => println!(
                "managed: {} {} · {}",
                channel.label(),
                state.version.as_deref().unwrap_or("?"),
                path.display()
            ),
            None => println!(
                "managed: {} — {}",
                channel.label(),
                if kr {
                    "설치되지 않음"
                } else {
                    "not installed"
                }
            ),
        }
        match state.last_check_unix {
            Some(at) => {
                let age_h = tools::ytdlp::now_unix().saturating_sub(at) / 3600;
                println!(
                    "{}: {age_h}h",
                    if kr { "마지막 확인" } else { "last check" }
                );
            }
            None => println!(
                "{}: {}",
                if kr { "마지막 확인" } else { "last check" },
                if kr { "없음" } else { "never" }
            ),
        }
    }

    println!("mpv: {}", cfg.tools.mpv_program());
    match tools::ytdlp_selection() {
        Some(_) => 0,
        None => 1,
    }
}

fn update() -> i32 {
    let cfg = config::Config::load();
    i18n::set_language(cfg.effective_language());
    let kr = i18n::is_korean();

    let outcome = block_on(async {
        tools::init(&cfg.tools).await;
        tools::ytdlp::check_and_update(&cfg.tools, &|event| match event {
            tools::ToolsEvent::Progress {
                channel,
                percent: Some(p),
            } => println!("  … {p:>3}% ({})", channel.label()),
            tools::ToolsEvent::Progress { channel, .. } => println!(
                "{} ({})…",
                if kr {
                    "yt-dlp 다운로드 중"
                } else {
                    "downloading yt-dlp"
                },
                channel.label()
            ),
            // Installed/Failed become the outcome lines below.
            tools::ToolsEvent::Installed { .. } | tools::ToolsEvent::Failed { .. } => {}
        })
        .await
    });
    let Some(outcome) = outcome else {
        eprintln!("ytt tools: failed to build async runtime");
        return 1;
    };

    match outcome {
        tools::ytdlp::UpdateOutcome::Installed { version } => {
            println!(
                "{}",
                if kr {
                    format!("yt-dlp {version} 설치 완료.")
                } else {
                    format!("yt-dlp {version} installed.")
                }
            );
            0
        }
        tools::ytdlp::UpdateOutcome::AlreadyCurrent => {
            let state = tools::ytdlp::load_state();
            println!(
                "{}",
                if kr {
                    format!(
                        "이미 최신입니다 ({}).",
                        state.version.as_deref().unwrap_or("?")
                    )
                } else {
                    format!(
                        "Already up to date ({}).",
                        state.version.as_deref().unwrap_or("?")
                    )
                }
            );
            0
        }
        tools::ytdlp::UpdateOutcome::Unavailable(e) => {
            eprintln!("ytt tools update: {e}");
            1
        }
    }
}
