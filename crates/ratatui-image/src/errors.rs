#[derive(Debug, thiserror::Error)]
pub enum Errors {
    #[error("Could not detect font size")]
    NoFontSize,
    #[error("Could not detect any graphics nor font capabilities")]
    NoCap,
    #[error("No response from stdin")]
    NoStdinResponse,
    #[error("Sixel error: {0}")]
    Sixel(String),
    #[error("Tmux error: {0}")]
    Tmux(&'static str),
    #[error("Io error: {0}")]
    Io(#[from] std::io::Error),
    /// Raw-mode teardown failed. Applications must not treat this as an unsupported capability:
    /// starting another TUI on top of the unrestored mode can corrupt the caller's shell.
    #[error("Terminal restore error: {0}")]
    TerminalRestore(Box<Errors>),
    #[error("Image error: {0}")]
    Image(#[from] image::error::ImageError),
}

#[cfg(not(windows))]
impl From<rustix::io::Errno> for Errors {
    fn from(errno: rustix::io::Errno) -> Self {
        Errors::Io(std::io::Error::from(errno))
    }
}

#[cfg(windows)]
impl From<windows::core::Error> for Errors {
    fn from(err: windows::core::Error) -> Self {
        Errors::Io(std::io::Error::new(
            std::io::ErrorKind::Other,
            err.to_string(),
        ))
    }
}
