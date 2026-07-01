use std::error::Error;

use ytm_tui::tray::launch;
use ytm_tui::tray::menu_model::{self, MenuEntry};
use ytm_tui::tray::status;
#[cfg(not(target_os = "macos"))]
use ytm_tui::tray::status::PollConfig;

fn main() -> Result<(), Box<dyn Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("--version") | Some("-V") => {
            println!("ytt-tray {}", env!("CARGO_PKG_VERSION"));
        }
        Some("--help") | Some("-h") => print_help(),
        Some("--once") => block_on(print_once()),
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
        None => run_default()?,
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
    println!("      --once       Print the current OS-neutral menu model and exit");
    println!("      --print-open-tui-plan");
    println!("                   Print terminal launch candidates without launching");
    println!("      --open-tui   Open ytt in a terminal");
    println!("  -V, --version    Print version and exit");
    println!("  -h, --help       Print this help and exit");
}

async fn print_once() {
    let update = status::poll_once().await;
    print_menu_update(&update);
}

fn run_default() -> Result<(), Box<dyn Error>> {
    #[cfg(target_os = "macos")]
    {
        ytm_tui::tray::platform::macos::run()
    }
    #[cfg(not(target_os = "macos"))]
    {
        block_on(run_polling());
        Ok(())
    }
}

#[cfg(not(target_os = "macos"))]
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
