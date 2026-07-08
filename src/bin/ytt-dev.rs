// Dev install alias for the main TUI binary. Keep this as a thin include so
// `cargo install --path . --force` installs both `ytt` and `ytt-dev`.
include!("../main.rs");
