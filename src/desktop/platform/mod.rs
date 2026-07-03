#[cfg(any(target_os = "macos", target_os = "windows"))]
mod main_window;
#[cfg(any(target_os = "macos", target_os = "windows"))]
mod panel_window;

#[cfg(target_os = "macos")]
pub mod macos;
#[cfg(target_os = "windows")]
pub mod windows;
