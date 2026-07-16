//! `ytt update` — report whether a newer YuTuTui! release exists and how to upgrade for
//! this machine's install method. One-shot, no terminal UI, like `ytt doctor`/`ytt tools`.
//! It never downloads or replaces the binary; it only prints guidance.

use crate::{config, i18n};

use super::{is_newer, resolved_install_method, update_instructions};

pub fn run(args: &[String]) -> i32 {
    if matches!(args.first().map(String::as_str), Some("--help" | "-h")) {
        help();
        return 0;
    }

    let cfg = config::Config::load();
    i18n::set_language(cfg.effective_language());
    let lang = i18n::current();

    let current = env!("CARGO_PKG_VERSION");
    let method = resolved_install_method();

    println!(
        "YuTuTui! {current} · {} {}",
        match lang {
            i18n::Language::Korean => "설치 방식:",
            i18n::Language::Japanese => "インストール方法:",
            _ => "installed via:",
        },
        method.label()
    );

    let latest = match block_on(super::resolve_latest()) {
        Some(Ok(tag)) => tag,
        Some(Err(e)) => {
            eprintln!(
                "{} {e}",
                match lang {
                    i18n::Language::Korean => "업데이트 확인 실패:",
                    i18n::Language::Japanese => "アップデート確認失敗:",
                    _ => "update check failed:",
                }
            );
            return 1;
        }
        None => {
            eprintln!("ytt update: failed to build async runtime");
            return 1;
        }
    };

    let display = latest.trim_start_matches(['v', 'V']);
    if !is_newer(&latest, current) {
        println!(
            "{}",
            match lang {
                i18n::Language::Korean => format!("이미 최신입니다 (최신 릴리즈 {display})."),
                i18n::Language::Japanese => format!("既に最新です (最新リリース {display})。"),
                _ => format!("You're on the latest release ({display})."),
            }
        );
        return 0;
    }

    println!(
        "{}",
        match lang {
            i18n::Language::Korean => format!("새 버전 v{display} 사용 가능 (현재 v{current})."),
            i18n::Language::Japanese =>
                format!("新バージョン v{display} が利用可能です (現在 v{current})。"),
            _ => format!("New version v{display} available (you have v{current})."),
        }
    );
    let ins = update_instructions(method);
    if let Some(command) = ins.command {
        println!("  {command}");
    } else {
        println!("  {}", ins.note);
    }
    println!("  {}", super::RELEASES_URL);
    // A newer release existing is not a process failure; exit 0.
    0
}

fn help() {
    println!("Usage: ytt update");
    println!();
    println!("Check whether a newer YuTuTui! release is available and print how to upgrade");
    println!("for your install method. Does not download or replace the binary.");
}

/// A current-thread runtime for the single release lookup (precedent: `ytt tools`).
fn block_on<F: std::future::Future>(fut: F) -> Option<F::Output> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .ok()
        .map(|rt| rt.block_on(fut))
}
