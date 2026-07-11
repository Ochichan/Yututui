#![cfg(feature = "desktop")]

use std::process::{Command, Output};

fn run(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_yututray"))
        .args(args)
        .output()
        .expect("yututray command should run")
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn help_documents_each_window_activation_intent() {
    let output = run(&["--help"]);
    assert!(output.status.success(), "stderr: {}", stderr(&output));

    let help = stdout(&output);
    for expected in [
        "Usage: yututray [OPTIONS]",
        "--background Run the tray companion from a startup entry (tray only)",
        "--mini       Open the tray mini player",
        "--main-window",
        "Open the experimental main window",
    ] {
        assert!(help.contains(expected), "missing {expected:?} in:\n{help}");
    }
}

#[test]
fn help_and_version_exit_without_starting_the_desktop_event_loop() {
    for args in [["--help"], ["-h"], ["--version"], ["-V"]] {
        let output = run(&args);
        assert!(
            output.status.success(),
            "{args:?} failed: {}",
            stderr(&output)
        );
    }
}

#[test]
fn unknown_option_is_a_usage_error() {
    let output = run(&["--not-a-real-option"]);
    assert_eq!(output.status.code(), Some(2));
    assert!(stderr(&output).contains("try `yututray --help`"));
}
