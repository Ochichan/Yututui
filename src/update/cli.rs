//! `ytt update` — report whether a newer ytm-tui release exists and how to upgrade for
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
    let kr = i18n::is_korean();

    let current = env!("CARGO_PKG_VERSION");
    let method = resolved_install_method();

    println!(
        "ytm-tui {current} · {} {}",
        if kr {
            "설치 방식:"
        } else {
            "installed via:"
        },
        method.label()
    );

    let latest = match block_on(super::resolve_latest()) {
        Some(Ok(tag)) => tag,
        Some(Err(e)) => {
            eprintln!(
                "{} {e}",
                if kr {
                    "업데이트 확인 실패:"
                } else {
                    "update check failed:"
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
            if kr {
                format!("이미 최신입니다 (최신 릴리즈 {display}).")
            } else {
                format!("You're on the latest release ({display}).")
            }
        );
        return 0;
    }

    println!(
        "{}",
        if kr {
            format!("새 버전 v{display} 사용 가능 (현재 v{current}).")
        } else {
            format!("New version v{display} available (you have v{current}).")
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
    println!("Check whether a newer ytm-tui release is available and print how to upgrade");
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
