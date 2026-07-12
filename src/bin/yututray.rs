// GUI subsystem in EVERY profile: a console window popping up alongside the app reads
// as broken to GUI users, debug builds included. Diagnostics live in the log file, and
// the CLI verbs still print via AttachConsole when launched from a terminal.
#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

use std::error::Error;

use yututui::desktop::launch;
use yututui::desktop::menu_model::{self, MenuEntry};
use yututui::desktop::single_instance::ActivationIntent;
use yututui::desktop::startup::{self, StartupStatus};
use yututui::desktop::status;
#[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
use yututui::desktop::status::PollConfig;
use yututui::{config::Config, i18n};

fn main() {
    if let Err(error) = try_main() {
        yututui::desktop::native_error::show("YuTuTui! Desktop", &error.to_string());
        std::process::exit(1);
    }
}

fn try_main() -> Result<(), Box<dyn Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    // The release binary is a GUI-subsystem executable (no console), which makes
    // every CLI verb print into the void when run from a terminal. Re-attach to the
    // parent console for anything that isn't the tray itself; harmless no-op when
    // launched from Explorer or a startup entry.
    #[cfg(windows)]
    {
        let launch_without_console = matches!(
            args.first().map(String::as_str),
            None | Some("--background") | Some("--mini")
        ) || args
            .first()
            .is_some_and(|arg| arg == "--main-window" && yututui::desktop::assets::DIST_EMBEDDED);
        if !launch_without_console {
            attach_parent_console();
        }
    }
    match args.first().map(String::as_str) {
        Some("--version") | Some("-V") => {
            println!("yututray {}", env!("CARGO_PKG_VERSION"));
        }
        Some("--help") | Some("-h") => print_help(),
        // Default to tray-only. The main window is still experimental and must be
        // opened explicitly so package-manager users do not see an unfinished GUI.
        Some("--background") => {
            run_default(ActivationIntent::EnsureTray, ActivationIntent::EnsureTray)?
        }
        Some("--mini") => run_default(ActivationIntent::ShowMini, ActivationIntent::ShowMini)?,
        Some("--main-window") if yututui::desktop::assets::DIST_EMBEDDED => {
            run_default(ActivationIntent::ShowMain, ActivationIntent::ShowMain)?
        }
        Some("--main-window") => {
            return Err(
                "the full GUI main window is not included in this build; use the tray mini player"
                    .into(),
            );
        }
        Some("--once") => block_on(print_once()),
        Some("--install-startup") => install_startup(),
        Some("--uninstall-startup") => uninstall_startup(),
        Some("--startup-status") => print_startup_status(),
        Some("--print-open-tui-plan") => {
            let ytt = launch::resolve_ytt_path();
            let plans =
                launch::candidate_plans_for(&ytt, std::env::var("TERMINAL").ok().as_deref());
            for plan in plans {
                println!("{} {}", plan.program, plan.args.join(" "));
            }
        }
        Some("--open-tui") => match launch::open_tui() {
            Ok(plan) => println!("launched {} {}", plan.program, plan.args.join(" ")),
            Err(e) => {
                eprintln!("yututray: {e}");
                std::process::exit(1);
            }
        },
        Some(other) => {
            eprintln!("yututray: unknown option `{other}` (try `yututray --help`)");
            std::process::exit(2);
        }
        // Compatibility: the first ordinary launch remains tray-only, while a second
        // ordinary launch surfaces the existing instance's mini player.
        None => run_default(ActivationIntent::EnsureTray, ActivationIntent::ShowMini)?,
    }
    Ok(())
}

fn print_help() {
    println!("yututray {}", env!("CARGO_PKG_VERSION"));
    println!();
    println!("Usage: yututray [OPTIONS]");
    println!();
    println!("Desktop companion for YuTuTui!.");
    println!();
    println!("Options:");
    println!("      --background Run the tray companion from a startup entry (tray only)");
    println!("      --mini       Open the tray mini player");
    if yututui::desktop::assets::DIST_EMBEDDED {
        println!("      --main-window");
        println!("                   Open the experimental main window");
    }
    println!("      --install-startup");
    println!("                   Enable login startup for yututray");
    println!("      --uninstall-startup");
    println!("                   Disable login startup for yututray");
    println!("      --startup-status");
    println!("                   Print the current startup registration state");
    println!("      --once       Print the current OS-neutral menu model and exit");
    println!("      --print-open-tui-plan");
    println!("                   Print terminal launch candidates without launching");
    println!("      --open-tui   Open ytt in a terminal");
    println!("  -V, --version    Print version and exit");
    println!("  -h, --help       Print this help and exit");
}

fn install_startup() {
    match startup::install() {
        Ok(command) => println!("installed startup entry: {command}"),
        Err(e) => {
            eprintln!("yututray: {e}");
            std::process::exit(1);
        }
    }
}

fn uninstall_startup() {
    match startup::uninstall() {
        Ok(()) => println!("removed startup entry"),
        Err(e) => {
            eprintln!("yututray: {e}");
            std::process::exit(1);
        }
    }
}

fn print_startup_status() {
    match startup::status() {
        Ok(StartupStatus::Enabled { command }) => println!("enabled: {command}"),
        Ok(StartupStatus::Disabled) => println!("disabled"),
        Ok(StartupStatus::Unsupported) => println!("unsupported"),
        Err(e) => {
            eprintln!("yututray: {e}");
            std::process::exit(1);
        }
    }
}

async fn print_once() {
    let update = status::poll_once().await;
    print_menu_update(&update);
}

fn run_default(
    initial_intent: ActivationIntent,
    secondary_intent: ActivationIntent,
) -> Result<(), Box<dyn Error>> {
    // Tray/menu/panel must have a useful locale even before a core session is online.
    // A v8 settings snapshot may refine this later, but local config is authoritative offline.
    let config = Config::load();
    i18n::set_language(config.effective_language());
    #[cfg(target_os = "macos")]
    {
        yututui::desktop::platform::macos::run(initial_intent, secondary_intent)
    }
    #[cfg(target_os = "windows")]
    {
        yututui::desktop::platform::windows::run(initial_intent, secondary_intent)
    }
    #[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
    {
        let _ = (initial_intent, secondary_intent);
        block_on(run_polling());
        Ok(())
    }
}

#[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
async fn run_polling() {
    eprintln!(
        "yututray foundation mode: polling ytt status until Ctrl-C. Native tray backends come next."
    );
    let shutdown = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    let mut last_summary: Option<String> = None;
    status::run_until_shutdown(
        PollConfig::default(),
        move |update| {
            let model = menu_model::build_menu(&update.state);
            let summary = model.summary_line();
            if last_summary.as_deref() != Some(summary.as_str()) {
                print_menu_update(&update);
                last_summary = Some(summary);
            }
        },
        shutdown,
    )
    .await;
}

#[cfg(windows)]
fn attach_parent_console() {
    use windows_sys::Win32::System::Console::{ATTACH_PARENT_PROCESS, AttachConsole};
    // Best-effort: fails when there is no parent console, which is fine.
    // SAFETY: ATTACH_PARENT_PROCESS is the documented sentinel; failure only means
    // this process has no attachable parent console.
    unsafe {
        AttachConsole(ATTACH_PARENT_PROCESS);
    }
}

fn block_on<F: std::future::Future>(future: F) -> F::Output {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("start yututray runtime");
    rt.block_on(future)
}

fn print_menu_update(update: &status::PollUpdate) {
    let model = menu_model::build_menu(&update.state);
    println!("{}", model.summary_line());
    if let Some(error) = &update.error {
        println!("error: {error}");
    }
    print_menu_entries(&model.entries, 0);
}

fn print_menu_entries(entries: &[MenuEntry], depth: usize) {
    let indent = "  ".repeat(depth);
    for entry in entries {
        match entry {
            MenuEntry::Separator => println!("{indent}---"),
            MenuEntry::Item(item) => {
                let marker = if item.enabled { " " } else { "x" };
                println!("{indent}[{marker}] {}", item.label);
            }
            MenuEntry::Submenu(submenu) => {
                let marker = if submenu.enabled { " " } else { "x" };
                println!("{indent}[{marker}] {}:", submenu.label);
                print_menu_entries(&submenu.entries, depth + 1);
            }
        }
    }
}
