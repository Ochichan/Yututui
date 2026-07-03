#![cfg_attr(
    all(target_os = "windows", not(debug_assertions)),
    windows_subsystem = "windows"
)]

use std::error::Error;

use ytm_tui::tray::launch;
use ytm_tui::tray::menu_model::{self, MenuEntry};
use ytm_tui::tray::startup::{self, StartupStatus};
use ytm_tui::tray::status;
#[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
use ytm_tui::tray::status::PollConfig;

fn main() -> Result<(), Box<dyn Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    // The release binary is a GUI-subsystem executable (no console), which makes
    // every CLI verb print into the void when run from a terminal. Re-attach to the
    // parent console for anything that isn't the tray itself; harmless no-op when
    // launched from Explorer or a startup entry.
    #[cfg(windows)]
    if !matches!(
        args.first().map(String::as_str),
        None | Some("--background") | Some("--main-window")
    ) {
        attach_parent_console();
    }
    match args.first().map(String::as_str) {
        Some("--version") | Some("-V") => {
            println!("ytt-tray {}", env!("CARGO_PKG_VERSION"));
        }
        Some("--help") | Some("-h") => print_help(),
        // Launched from a startup entry → tray only; launched from the app icon (no args)
        // or with --main-window → open the main window (docs/gui/03 §1.4).
        Some("--background") => run_default(false)?,
        Some("--main-window") => run_default(true)?,
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
                eprintln!("ytt-tray: {e}");
                std::process::exit(1);
            }
        },
        Some(other) => {
            eprintln!("ytt-tray: unknown option `{other}` (try `ytt-tray --help`)");
            std::process::exit(2);
        }
        None => run_default(true)?,
    }
    Ok(())
}

fn print_help() {
    println!("ytt-tray {}", env!("CARGO_PKG_VERSION"));
    println!();
    println!("Usage: ytt-tray [OPTIONS]");
    println!();
    println!("Desktop companion for ytm-tui.");
    println!();
    println!("Options:");
    println!("      --background Run the tray companion from a startup entry (tray only)");
    println!("      --main-window");
    println!("                   Open the main window (default when launched from the app icon)");
    println!("      --install-startup");
    println!("                   Enable login startup for ytt-tray");
    println!("      --uninstall-startup");
    println!("                   Disable login startup for ytt-tray");
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
            eprintln!("ytt-tray: {e}");
            std::process::exit(1);
        }
    }
}

fn uninstall_startup() {
    match startup::uninstall() {
        Ok(()) => println!("removed startup entry"),
        Err(e) => {
            eprintln!("ytt-tray: {e}");
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
            eprintln!("ytt-tray: {e}");
            std::process::exit(1);
        }
    }
}

async fn print_once() {
    let update = status::poll_once().await;
    print_menu_update(&update);
}

fn run_default(open_main: bool) -> Result<(), Box<dyn Error>> {
    #[cfg(target_os = "macos")]
    {
        ytm_tui::tray::platform::macos::run(open_main)
    }
    #[cfg(target_os = "windows")]
    {
        ytm_tui::tray::platform::windows::run(open_main)
    }
    #[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
    {
        let _ = open_main;
        block_on(run_polling());
        Ok(())
    }
}

#[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
async fn run_polling() {
    eprintln!(
        "ytt-tray foundation mode: polling ytt status until Ctrl-C. Native tray backends come next."
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
    unsafe {
        AttachConsole(ATTACH_PARENT_PROCESS);
    }
}

fn block_on<F: std::future::Future>(future: F) -> F::Output {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("start ytt-tray runtime");
    rt.block_on(future)
}

fn print_menu_update(update: &status::PollUpdate) {
    let model = menu_model::build_menu(&update.state);
    println!("{}", model.summary_line());
    if let Some(error) = &update.error {
        println!("error: {error}");
    }
    for entry in &model.entries {
        match entry {
            MenuEntry::Separator => println!("---"),
            MenuEntry::Item(item) => {
                let marker = if item.enabled { " " } else { "x" };
                println!("[{marker}] {}", item.label);
            }
        }
    }
}
