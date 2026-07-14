//! Helper module to build a protocol, and swap protocols at runtime

use std::{
    env,
    io::{self, Read, Write},
    sync::Arc,
    time::{Duration, Instant},
};

use crate::{
    FontSize, Resize, Result,
    errors::Errors,
    protocol::{
        Protocol, StatefulProtocol, StatefulProtocolType,
        halfblocks::Halfblocks,
        iterm2::Iterm2,
        kitty::{Kitty, StatefulKitty},
        sixel::Sixel,
    },
};
use cap_parser::{Parser, QueryStdioOptions, Response};
use image::{DynamicImage, Rgba};
use rand::random;
use ratatui::layout::Size;
#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

pub mod cap_parser;

#[derive(Debug, PartialEq, Clone)]
pub enum Capability {
    /// Reports supporting kitty graphics protocol.
    Kitty,
    /// Reports supporting sixel graphics protocol.
    Sixel,
    /// Reports supporting rectangular ops.
    RectangularOps,
    /// Reports font size in pixels.
    CellSize(Option<(u16, u16)>),
    /// Reports supporting text sizing protocol.
    TextSizingProtocol,
    /// Reports a background color.
    Background(u8, u8, u8),
}

const STDIN_READ_TIMEOUT_MILLIS: u64 = 2000;

// yututui patch: Konsole's 26.08 line includes the Sixel placement cleanup needed for TUI redraws.
const KONSOLE_SIXEL_TUI_MIN_VERSION: u32 = 260_800;

#[derive(Clone, Debug)]
pub struct Picker {
    font_size: FontSize,
    protocol_type: ProtocolType,
    background_color: Option<Rgba<u8>>,
    pub(crate) is_tmux: bool,
    capabilities: Vec<Capability>,
}

/// Serde-friendly protocol-type enum for [Picker].
#[derive(PartialEq, Clone, Debug, Copy)]
#[cfg_attr(
    feature = "serde",
    derive(Deserialize, Serialize),
    serde(rename_all = "lowercase")
)]
pub enum ProtocolType {
    Halfblocks,
    Sixel,
    Kitty,
    Iterm2,
}

impl ProtocolType {
    pub fn next(&self) -> ProtocolType {
        match self {
            ProtocolType::Halfblocks => ProtocolType::Sixel,
            ProtocolType::Sixel => ProtocolType::Kitty,
            ProtocolType::Kitty => ProtocolType::Iterm2,
            ProtocolType::Iterm2 => ProtocolType::Halfblocks,
        }
    }
}

/// Helper for building widgets
impl Picker {
    /// Query terminal stdio for graphics capabilities and font-size with some escape sequences.
    ///
    /// This writes and reads from stdio momentarily. WARNING: this method should be called after
    /// entering alternate screen but before reading terminal events.
    ///
    /// # Example
    /// ```rust
    /// use ratatui_image::picker::Picker;
    /// let mut picker = Picker::from_query_stdio();
    /// ```
    ///
    pub fn from_query_stdio() -> Result<Self> {
        Picker::from_query_stdio_with_options(QueryStdioOptions::default())
    }

    /// This should ONLY be used if [Capability::TextSizingProtocol] is needed for some external
    /// reason.
    ///
    /// Query for additional capabilities, currently supports querying for [Text Sizing Protocol].
    ///
    /// The result can be checked by searching for [Capability::TextSizingProtocol] in [Picker::capabilities].
    ///
    /// [Text Sizing Protocol] <https://sw.kovidgoyal.net/kitty/text-sizing-protocol//>
    pub fn from_query_stdio_with_options(options: QueryStdioOptions) -> Result<Self> {
        // Detect tmux, and only if positive then take some risky guess for iTerm2 support.
        let (is_tmux, tmux_proto) = detect_tmux_and_outer_protocol_from_env();

        static DEFAULT_PICKER: Picker = Picker {
            // This is completely arbitrary. For halfblocks, it doesn't have to be precise
            // since we're not rendering pixels. It should be roughly 1:2 ratio, and some
            // reasonable size.
            font_size: FontSize::new(10, 20),
            background_color: None,
            protocol_type: ProtocolType::Halfblocks,
            is_tmux: false,
            capabilities: Vec::new(),
        };

        let mut options_with_blacklist = options;
        let wezterm_executable = env::var("WEZTERM_EXECUTABLE").ok();
        let konsole_version = env::var("KONSOLE_VERSION").ok();
        let term = env::var("TERM").ok();
        let require_reported_cell_size_for_konsole_sixel = konsole_supports_sixel_tui_redraws(
            wezterm_executable.as_deref(),
            konsole_version.as_deref(),
            term.as_deref(),
        );
        for protocol in terminal_protocol_blacklist(
            wezterm_executable.as_deref(),
            konsole_version.as_deref(),
            term.as_deref(),
        ) {
            if !options_with_blacklist
                .blacklist_protocols
                .contains(&protocol)
            {
                options_with_blacklist.blacklist_protocols.push(protocol);
            }
        }

        // Write and read to stdin to query protocol capabilities and font-size.
        match query_with_timeout(is_tmux, options_with_blacklist) {
            Ok((capability_proto, font_size, caps)) => {
                let iterm2_proto = iterm2_from_env();
                let capability_proto = require_reported_cell_size_for_sixel(
                    capability_proto,
                    &caps,
                    require_reported_cell_size_for_konsole_sixel,
                );

                // IO-based detection is authoritative; env-based hints are fallbacks
                // (env vars like KITTY_WINDOW_ID can be stale in tmux sessions).
                let protocol_type = capability_proto
                    .or(tmux_proto)
                    .or(iterm2_proto)
                    .unwrap_or(ProtocolType::Halfblocks);

                if let Some(font_size) = font_size {
                    Ok(Self {
                        font_size,
                        background_color: None,
                        protocol_type,
                        is_tmux,
                        capabilities: caps,
                    })
                } else {
                    let mut p = DEFAULT_PICKER.clone();
                    p.is_tmux = is_tmux;
                    Ok(p)
                }
            }
            Err(Errors::NoCap | Errors::NoStdinResponse | Errors::NoFontSize) => {
                let mut p = DEFAULT_PICKER.clone();
                p.is_tmux = is_tmux;
                Ok(p)
            }
            Err(err) => Err(err),
        }
    }

    /// Create a picker that is guaranteed to only work with Halfblocks.
    ///
    /// # Example
    /// ```rust
    /// use ratatui_image::picker::Picker;
    ///
    /// let mut picker = Picker::halfblocks();
    /// ```
    pub fn halfblocks() -> Self {
        // Detect tmux, ignore iTerm2 as we don't have font-size.
        let (is_tmux, _tmux_proto) = detect_tmux_and_outer_protocol_from_env();

        Self {
            font_size: FontSize::new(10, 20),
            background_color: None,
            protocol_type: ProtocolType::Halfblocks,
            is_tmux,
            capabilities: Vec::new(),
        }
    }

    /// Create a picker from a given terminal [FontSize].
    #[deprecated(
        since = "9.0.0",
        note = "use `from_query_stdio` or `halfblocks` instead"
    )]
    pub fn from_fontsize(font_size: FontSize) -> Self {
        // Detect tmux, and if positive then take some risky guess for iTerm2 support.
        let (is_tmux, tmux_proto) = detect_tmux_and_outer_protocol_from_env();

        // Disregard protocol-from-capabilities if some env var says that we could try iTerm2.
        let iterm2_proto = iterm2_from_env();

        let protocol_type = tmux_proto
            .or(iterm2_proto)
            .unwrap_or(ProtocolType::Halfblocks);

        Self {
            font_size,
            background_color: None,
            protocol_type,
            is_tmux,
            capabilities: Vec::new(),
        }
    }

    /// Returns the current protocol type.
    pub fn protocol_type(&self) -> ProtocolType {
        self.protocol_type
    }

    /// Force a protocol type.
    pub fn set_protocol_type(&mut self, protocol_type: ProtocolType) {
        self.protocol_type = protocol_type;
    }

    /// Returns the [FontSize] detected by [Picker::from_query_stdio].
    pub fn font_size(&self) -> FontSize {
        self.font_size
    }

    /// Change the default background color (transparent black).
    pub fn set_background_color<T: Into<Rgba<u8>>>(&mut self, background_color: Option<T>) {
        self.background_color = background_color.map(Into::into);
    }

    /// Returns the capabilities detected by [Picker::from_query_stdio].
    pub fn capabilities(&self) -> &Vec<Capability> {
        &self.capabilities
    }

    /// Returns a new protocol.
    ///
    /// The image must match the given area at the terminal's current font size.
    pub(crate) fn new_protocol_raw(&self, image: DynamicImage, size: Size) -> Result<Protocol> {
        match self.protocol_type {
            ProtocolType::Halfblocks => Ok(Protocol::Halfblocks(Halfblocks::new(image, size)?)),
            ProtocolType::Sixel => Ok(Protocol::Sixel(Sixel::new(image, size, self.is_tmux)?)),
            ProtocolType::Kitty => Ok(Protocol::Kitty(Kitty::new(
                image,
                size,
                rand::random(),
                self.is_tmux,
            )?)),
            ProtocolType::Iterm2 => Ok(Protocol::ITerm2(Iterm2::new(image, size, self.is_tmux)?)),
        }
    }

    /// Returns a new protocol for [`crate::Image`] widgets that fits into the given size.
    pub fn new_protocol(
        &self,
        image: DynamicImage,
        size: Size,
        resize: Resize,
    ) -> Result<Protocol> {
        let desired =
            Resize::round_pixel_size_to_cells(image.width(), image.height(), self.font_size);
        let (image, area) =
            match resize.needs_resize(&image, Some(desired), self.font_size, None, size, false) {
                Some(area) => {
                    let image = resize.resize(&image, self.font_size, area, self.background_color);
                    (image, area)
                }
                None => (image, desired),
            };

        self.new_protocol_raw(image, area)
    }

    /// Returns a new *stateful* protocol for [`crate::StatefulImage`] widgets.
    pub fn new_resize_protocol(&self, image: DynamicImage) -> StatefulProtocol {
        self.new_resize_protocol_shared(Arc::new(image))
    }

    /// Returns a new stateful protocol sharing its immutable decoded source pixels.
    pub fn new_resize_protocol_shared(&self, image: Arc<DynamicImage>) -> StatefulProtocol {
        self.new_resize_protocol_shared_with_kitty_z_index(image, None)
    }

    /// Returns a new *stateful* protocol, overriding Kitty's z-index when Kitty is selected.
    ///
    /// yututui uses album art as a background layer with a very low Kitty z-index, but small
    /// foreground graphics inside opaque popups need the normal text layer instead.
    pub fn new_resize_protocol_with_kitty_z_index(
        &self,
        image: DynamicImage,
        kitty_z_index: Option<i32>,
    ) -> StatefulProtocol {
        self.new_resize_protocol_shared_with_kitty_z_index(Arc::new(image), kitty_z_index)
    }

    /// Shared-source variant of [`Self::new_resize_protocol_with_kitty_z_index`].
    pub fn new_resize_protocol_shared_with_kitty_z_index(
        &self,
        image: Arc<DynamicImage>,
        kitty_z_index: Option<i32>,
    ) -> StatefulProtocol {
        let protocol_type = match self.protocol_type {
            ProtocolType::Halfblocks => StatefulProtocolType::Halfblocks(Halfblocks::default()),
            ProtocolType::Sixel => StatefulProtocolType::Sixel(Sixel {
                is_tmux: self.is_tmux,
                ..Sixel::default()
            }),
            ProtocolType::Kitty => StatefulProtocolType::Kitty(match kitty_z_index {
                Some(z_index) => StatefulKitty::new_with_z_index(random(), self.is_tmux, z_index),
                None => StatefulKitty::new(random(), self.is_tmux),
            }),
            ProtocolType::Iterm2 => StatefulProtocolType::ITerm2(Iterm2 {
                is_tmux: self.is_tmux,
                ..Iterm2::default()
            }),
        };
        StatefulProtocol::new_shared(image, self.font_size, self.background_color, protocol_type)
    }
}

// yututui patch: keep older/unknown Konsole versions conservative, and capability-gate 26.08+.
fn terminal_protocol_blacklist(
    wezterm_executable: Option<&str>,
    konsole_version: Option<&str>,
    term: Option<&str>,
) -> Vec<ProtocolType> {
    let is_wezterm = wezterm_executable.is_some_and(|value| !value.is_empty());
    if is_wezterm {
        // WezTerm could use Sixel, but iTerm2 (detected later) is better. It also does not
        // implement the placeholder part of Kitty correctly.
        return vec![ProtocolType::Kitty, ProtocolType::Sixel];
    }

    let is_konsole = konsole_version.is_some_and(|value| !value.is_empty())
        || term.is_some_and(|value| value.to_ascii_lowercase().contains("konsole"));
    if !is_konsole {
        return Vec::new();
    }

    if konsole_supports_sixel_tui_redraws(wezterm_executable, konsole_version, term) {
        // Konsole's Kitty implementation still lacks Unicode placeholders. Sixel remains subject
        // to the normal DA1 capability and cell-size response checks below.
        vec![ProtocolType::Kitty]
    } else {
        vec![ProtocolType::Kitty, ProtocolType::Sixel]
    }
}

fn konsole_supports_sixel_tui_redraws(
    wezterm_executable: Option<&str>,
    konsole_version: Option<&str>,
    term: Option<&str>,
) -> bool {
    let is_wezterm = wezterm_executable.is_some_and(|value| !value.is_empty());
    let is_konsole = konsole_version.is_some_and(|value| !value.is_empty())
        || term.is_some_and(|value| value.to_ascii_lowercase().contains("konsole"));

    !is_wezterm
        && is_konsole
        && konsole_version
            .and_then(|version| version.trim().parse::<u32>().ok())
            .is_some_and(|version| version >= KONSOLE_SIXEL_TUI_MIN_VERSION)
}

fn require_reported_cell_size_for_sixel(
    protocol_type: Option<ProtocolType>,
    capabilities: &[Capability],
    required: bool,
) -> Option<ProtocolType> {
    if required
        && protocol_type == Some(ProtocolType::Sixel)
        && !capabilities
            .iter()
            .any(|capability| matches!(capability, Capability::CellSize(Some(_))))
    {
        None
    } else {
        protocol_type
    }
}

fn detect_tmux_and_outer_protocol_from_env() -> (bool, Option<ProtocolType>) {
    // Check if we're inside tmux.
    if !env::var("TERM").is_ok_and(|term| term.starts_with("tmux"))
        && !env::var("TERM_PROGRAM").is_ok_and(|term_program| term_program == "tmux")
    {
        return (false, None);
    }

    let _ = std::process::Command::new("tmux")
        .args(["set", "-p", "allow-passthrough", "on"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .and_then(|mut child| child.wait()); // wait(), for check_device_attrs.

    // Crude guess based on the *existence* of some magic program specific env vars.
    // Note: kitty is detected via io query (which works through tmux passthrough),
    // not env vars, since KITTY_WINDOW_ID is often stale in tmux sessions.
    const OUTER_TERM_HINTS: [(&str, ProtocolType); 2] = [
        ("ITERM_SESSION_ID", ProtocolType::Iterm2),
        ("WEZTERM_EXECUTABLE", ProtocolType::Iterm2),
    ];
    for (hint, proto) in OUTER_TERM_HINTS {
        if env::var(hint).is_ok_and(|s| !s.is_empty()) {
            return (true, Some(proto));
        }
    }
    (true, None)
}

fn iterm2_from_env() -> Option<ProtocolType> {
    if env::var("TERM_PROGRAM").is_ok_and(|term_program| {
        term_program.contains("iTerm")
            || term_program.contains("WezTerm")
            || term_program.contains("mintty")
            || term_program.contains("vscode")
            || term_program.contains("Tabby")
            || term_program.contains("Hyper")
            || term_program.contains("rio")
            || term_program.contains("Bobcat")
            || term_program.contains("WarpTerminal")
    }) {
        return Some(ProtocolType::Iterm2);
    }
    if env::var("LC_TERMINAL").is_ok_and(|lc_term| lc_term.contains("iTerm")) {
        return Some(ProtocolType::Iterm2);
    }
    None
}

#[cfg(not(windows))]
fn enable_raw_mode() -> Result<impl FnOnce() -> Result<()>> {
    use rustix::termios::{self, LocalModes, OptionalActions};

    let stdin = io::stdin();
    let mut termios = termios::tcgetattr(&stdin)?;
    let termios_original = termios.clone();

    // Disable canonical mode to read without waiting for Enter, disable echoing.
    termios.local_modes &= !LocalModes::ICANON;
    termios.local_modes &= !LocalModes::ECHO;
    termios::tcsetattr(&stdin, OptionalActions::Drain, &termios)?;

    Ok(move || {
        Ok(termios::tcsetattr(
            io::stdin(),
            OptionalActions::Now,
            &termios_original,
        )?)
    })
}

#[cfg(windows)]
fn enable_raw_mode() -> Result<impl FnOnce() -> Result<()>> {
    use windows::{
        Win32::{
            Foundation::{GENERIC_READ, GENERIC_WRITE, HANDLE},
            Storage::FileSystem::{
                self, FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
            },
            System::Console::{
                self, CONSOLE_MODE, ENABLE_ECHO_INPUT, ENABLE_LINE_INPUT, ENABLE_PROCESSED_INPUT,
            },
        },
        core::PCWSTR,
    };

    let utf16: Vec<u16> = "CONIN$\0".encode_utf16().collect();
    let utf16_ptr: *const u16 = utf16.as_ptr();

    // SAFETY: `utf16_ptr` points to a NUL-terminated "CONIN$" buffer that lives for
    // the call; CreateFileW returns a Result-wrapped console input handle.
    let in_handle = unsafe {
        FileSystem::CreateFileW(
            PCWSTR(utf16_ptr),
            (GENERIC_READ | GENERIC_WRITE).0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            FILE_FLAGS_AND_ATTRIBUTES(0),
            HANDLE::default(),
        )
    }?;

    let mut original_in_mode = CONSOLE_MODE::default();
    // SAFETY: `in_handle` is the console input handle returned by CreateFileW and
    // `original_in_mode` is valid output storage.
    unsafe { Console::GetConsoleMode(in_handle, &mut original_in_mode) }?;

    let requested_in_modes = !ENABLE_ECHO_INPUT & !ENABLE_LINE_INPUT & !ENABLE_PROCESSED_INPUT;
    let in_mode = original_in_mode & requested_in_modes;
    // SAFETY: `in_handle` is a console input handle and `in_mode` is derived from the
    // current mode by clearing documented raw-input flags.
    unsafe { Console::SetConsoleMode(in_handle, in_mode) }?;

    Ok(move || {
        // SAFETY: restores the saved mode on the same console input handle; failure is
        // returned to the caller.
        unsafe { Console::SetConsoleMode(in_handle, original_in_mode) }?;
        Ok(())
    })
}

#[cfg(not(windows))]
fn font_size_fallback() -> Option<FontSize> {
    use rustix::termios::{self, Winsize};

    let winsize = termios::tcgetwinsize(io::stdout()).ok()?;
    let Winsize {
        ws_xpixel: x,
        ws_ypixel: y,
        ws_col: cols,
        ws_row: rows,
    } = winsize;
    if x == 0 || y == 0 || cols == 0 || rows == 0 {
        return None;
    }

    Some(FontSize::new(x / cols, y / rows))
}

#[cfg(windows)]
fn font_size_fallback() -> Option<FontSize> {
    None
}

/// Query the terminal by writing capability/font-size escape sequences to stdout and reading the
/// responses from stdin, bounded by `timeout`.
///
/// The terminal must already be in "raw mode" (no echo, no canonical/line buffering) so the
/// responses can be read byte-by-byte; the caller restores the previous mode afterwards.
///
/// Several control sequences are sent at once:
/// - `_Gi=...`: Kitty graphics support.
/// - `[c`: Capabilities including sixels.
/// - `[16t`: Cell-size.
/// - `[5n`: Device Status Report, implemented by all terminals. Its `[0n` answer is the
///   terminator we stop on, so a cooperating terminal never makes us read forever.
///
/// We also stop once `timeout` elapses with no further input — so a terminal that drops a response
/// cannot hang us. This runs entirely on the calling thread: there is no background reader that
/// could outlive the call and steal input from the event loop later.
fn query_stdio_capabilities(
    is_tmux: bool,
    options: QueryStdioOptions,
    timeout: Duration,
) -> Result<(Option<ProtocolType>, Option<FontSize>, Vec<Capability>)> {
    let query = Parser::query(is_tmux, options);
    io::stdout().write_all(query.as_bytes())?;
    io::stdout().flush()?;

    let deadline = Instant::now() + timeout;
    let mut parser = Parser::new();
    let mut responses = vec![];
    'out: loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() || !wait_readable(remaining)? {
            // Timed out waiting for (more) response. Drain anything already buffered so stray
            // bytes don't leak into the subsequent event loop, then give up on the rest.
            drain_pending_input();
            break 'out;
        }

        let mut charbuf: [u8; 50] = [0; 50];
        let read = io::stdin().read(&mut charbuf)?;
        if read == 0 {
            break 'out; // EOF on stdin
        }

        for ch in charbuf.iter().take(read) {
            let mut more_caps = parser.push(char::from(*ch));
            match more_caps[..] {
                [Response::Status] => {
                    // DSR terminator: the full response has arrived, nothing trails it.
                    break 'out;
                }
                _ => responses.append(&mut more_caps),
            }
        }
    }

    interpret_parser_responses(responses)
}

/// Block until stdin is readable or `timeout` elapses; returns whether stdin became readable.
#[cfg(not(windows))]
fn wait_readable(timeout: Duration) -> Result<bool> {
    use rustix::event::{PollFd, PollFlags, poll};

    let stdin = rustix::stdio::stdin();
    let mut fds = [PollFd::new(&stdin, PollFlags::IN)];
    let millis: i32 = timeout.as_millis().try_into().unwrap_or(i32::MAX);
    poll(&mut fds, millis)?;
    Ok(fds[0].revents().contains(PollFlags::IN))
}

/// Block until stdin is readable or `timeout` elapses; returns whether stdin became readable.
#[cfg(windows)]
fn wait_readable(timeout: Duration) -> Result<bool> {
    use windows::Win32::Foundation::WAIT_OBJECT_0;
    use windows::Win32::System::Console::{GetStdHandle, STD_INPUT_HANDLE};
    use windows::Win32::System::Threading::WaitForSingleObject;

    // SAFETY: STD_INPUT_HANDLE is the documented selector and the Result captures
    // invalid-handle failures.
    let handle = unsafe { GetStdHandle(STD_INPUT_HANDLE) }?;
    let millis: u32 = timeout.as_millis().try_into().unwrap_or(u32::MAX);
    // SAFETY: waiting on the console input handle is a bounded read readiness probe;
    // timeout conversion saturates and the result is compared to WAIT_OBJECT_0.
    let result = unsafe { WaitForSingleObject(handle, millis) };
    Ok(result == WAIT_OBJECT_0)
}

/// Best-effort, non-blocking drain of any bytes still buffered on stdin (e.g. a late or partial
/// capability response) so they are not re-interpreted as input events by the event loop.
fn drain_pending_input() {
    while matches!(wait_readable(Duration::ZERO), Ok(true)) {
        let mut buf = [0u8; 64];
        match io::stdin().read(&mut buf) {
            Ok(n) if n > 0 => continue,
            _ => break,
        }
    }
}

fn interpret_parser_responses(
    responses: Vec<Response>,
) -> Result<(Option<ProtocolType>, Option<FontSize>, Vec<Capability>)> {
    if responses.is_empty() {
        return Err(Errors::NoCap);
    }

    let mut capabilities = Vec::new();

    let mut proto = None;
    let mut font_size = None;

    let mut cursor_position_reports = vec![];
    for response in &responses {
        if let Some(capability) = match response {
            Response::Kitty => {
                proto = Some(ProtocolType::Kitty);
                Some(Capability::Kitty)
            }
            Response::Sixel => {
                if proto.is_none() {
                    // Only if kitty is not supported.
                    proto = Some(ProtocolType::Sixel);
                }
                Some(Capability::Sixel)
            }
            Response::RectangularOps => Some(Capability::RectangularOps),
            Response::CellSize(cell_size) => {
                if let Some((w, h)) = cell_size {
                    font_size = Some((*w, *h).into());
                }
                Some(Capability::CellSize(*cell_size))
            }
            Response::CursorPositionReport(x, y) => {
                cursor_position_reports.push((x, y));
                None
            }
            Response::Background(r, g, b) => Some(Capability::Background(*r, *g, *b)),
            Response::Status => None,
        } {
            capabilities.push(capability);
        }
    }

    // In case some terminal didn't support the cell-size query.
    font_size = font_size.or_else(font_size_fallback);

    if let [(x1, _y1), (x2, _y2), (x3, _y3)] = cursor_position_reports[..] {
        // Test if the cursor advanced exactly two columns (instead of one) on both the width and
        // scaling queries of the protocol.
        // The documentation is a bit ambiguous, as it only says the cursor positions "need to be
        // different from each other".
        // However from my testing on Kitty and other terminals that do not support the feature,
        // the cursor always advances at least one column since it is printing a space, so the CPRs
        // will always be different from each other (unless we would move the cursor to a known
        // position or something like that - and this also begs the question of needing to do this
        // anyway, for the edge case of the cursor being at the very end of a line).
        // My interpretation is that the cursor should advance 2 columns, instead of one, with both
        // queries, and only then can we interpret it as supported.
        // The Foot terminal notably reports a 2 column movement but fortunately only for the `w=2`
        // query.
        //
        // The row part can be ignored.
        if *x2 == x1 + 2 && *x3 == x2 + 2 {
            capabilities.push(Capability::TextSizingProtocol);
        }
    }

    Ok((proto, font_size, capabilities))
}

fn query_with_timeout(
    is_tmux: bool,
    options: QueryStdioOptions,
) -> Result<(Option<ProtocolType>, Option<FontSize>, Vec<Capability>)> {
    let timeout = options.timeout;

    // Put the tty in raw mode so the query responses aren't echoed or line-buffered, run the
    // query, then restore the previous mode BEFORE returning. Doing this synchronously — with no
    // background thread — is what fixes the kitty startup corruption: the old design signalled
    // completion over a channel and only restored the terminal mode afterwards, so the caller
    // (crossterm's `enable_raw_mode`) could observe, and save as its restore target, a termios
    // that was still raw. That left the app running in cooked mode (every keystroke echoed and
    // line-buffered) and the user's shell in raw mode after exit. A detached reader could also
    // outlive a timeout and keep stealing bytes from the event loop; there is none now.
    let disable_raw_mode = enable_raw_mode()?;
    let result = query_stdio_capabilities(is_tmux, options, timeout);
    let restored = disable_raw_mode();
    let caps = result?;
    restored?;
    Ok(caps)
}

#[cfg(test)]
mod tests {
    use std::assert_eq;

    use crate::picker::{Capability, Picker, ProtocolType};

    use super::{
        cap_parser::{Parser, QueryStdioOptions, Response},
        interpret_parser_responses, require_reported_cell_size_for_sixel,
        terminal_protocol_blacklist,
    };

    #[test]
    fn test_cycle_protocol() {
        let mut proto = ProtocolType::Halfblocks;
        proto = proto.next();
        assert_eq!(proto, ProtocolType::Sixel);
        proto = proto.next();
        assert_eq!(proto, ProtocolType::Kitty);
        proto = proto.next();
        assert_eq!(proto, ProtocolType::Iterm2);
        proto = proto.next();
        assert_eq!(proto, ProtocolType::Halfblocks);
    }

    #[test]
    fn test_from_query_stdio_no_hang() {
        let _ = Picker::from_query_stdio();
    }

    #[test]
    fn test_terminal_protocol_blacklist() {
        struct Case {
            name: &'static str,
            wezterm_executable: Option<&'static str>,
            konsole_version: Option<&'static str>,
            term: Option<&'static str>,
            expected: Vec<ProtocolType>,
        }

        let cases = [
            Case {
                name: "unrelated terminal",
                wezterm_executable: None,
                konsole_version: None,
                term: Some("xterm-256color"),
                expected: vec![],
            },
            Case {
                name: "WezTerm keeps Kitty and Sixel blacklisted",
                wezterm_executable: Some("/Applications/WezTerm.app/Contents/MacOS/wezterm-gui"),
                konsole_version: None,
                term: Some("xterm-256color"),
                expected: vec![ProtocolType::Kitty, ProtocolType::Sixel],
            },
            Case {
                name: "Konsole detected from TERM without a version",
                wezterm_executable: None,
                konsole_version: None,
                term: Some("konsole-256color"),
                expected: vec![ProtocolType::Kitty, ProtocolType::Sixel],
            },
            Case {
                name: "empty Konsole version is not a hint",
                wezterm_executable: None,
                konsole_version: Some(""),
                term: Some("xterm-256color"),
                expected: vec![],
            },
            Case {
                name: "invalid Konsole version stays conservative",
                wezterm_executable: None,
                konsole_version: Some("26.08"),
                term: Some("xterm-256color"),
                expected: vec![ProtocolType::Kitty, ProtocolType::Sixel],
            },
            Case {
                name: "Konsole before 26.08 stays conservative",
                wezterm_executable: None,
                konsole_version: Some("260799"),
                term: Some("xterm-256color"),
                expected: vec![ProtocolType::Kitty, ProtocolType::Sixel],
            },
            Case {
                name: "Konsole 26.08 allows Sixel capability queries",
                wezterm_executable: None,
                konsole_version: Some("260800"),
                term: Some("xterm-256color"),
                expected: vec![ProtocolType::Kitty],
            },
            Case {
                name: "newer Konsole allows Sixel capability queries",
                wezterm_executable: None,
                konsole_version: Some("260801"),
                term: Some("xterm-256color"),
                expected: vec![ProtocolType::Kitty],
            },
            Case {
                name: "WezTerm policy wins over a new Konsole hint",
                wezterm_executable: Some("wezterm-gui"),
                konsole_version: Some("260800"),
                term: Some("konsole-256color"),
                expected: vec![ProtocolType::Kitty, ProtocolType::Sixel],
            },
        ];

        for case in cases {
            assert_eq!(
                terminal_protocol_blacklist(
                    case.wezterm_executable,
                    case.konsole_version,
                    case.term,
                ),
                case.expected,
                "{}",
                case.name
            );
        }
    }

    #[test]
    fn test_konsole_sixel_query_is_version_gated() {
        let query_for = |konsole_version| {
            let options = QueryStdioOptions {
                blacklist_protocols: terminal_protocol_blacklist(
                    None,
                    Some(konsole_version),
                    Some("konsole-256color"),
                ),
                ..QueryStdioOptions::default()
            };
            Parser::query(false, options)
        };

        let old_konsole_query = query_for("260799");
        assert!(!old_konsole_query.contains("_Gi="));
        assert!(!old_konsole_query.contains("\x1b[c"));

        let new_konsole_query = query_for("260800");
        assert!(!new_konsole_query.contains("_Gi="));
        assert!(new_konsole_query.contains("\x1b[c"));
    }

    #[test]
    fn test_terminal_blacklist_preserves_caller_entries() {
        let mut blacklist = vec![ProtocolType::Iterm2];
        for protocol in terminal_protocol_blacklist(None, Some("260799"), None) {
            if !blacklist.contains(&protocol) {
                blacklist.push(protocol);
            }
        }

        assert_eq!(
            blacklist,
            vec![
                ProtocolType::Iterm2,
                ProtocolType::Kitty,
                ProtocolType::Sixel
            ]
        );
    }

    #[test]
    fn test_konsole_sixel_requires_reported_cell_size() {
        let (sixel_without_cell_size, _, capabilities) =
            interpret_parser_responses(vec![Response::Sixel]).unwrap();

        assert_eq!(
            require_reported_cell_size_for_sixel(sixel_without_cell_size, &capabilities, true),
            None
        );

        let (sixel_without_valid_cell_size, _, capabilities) =
            interpret_parser_responses(vec![Response::Sixel, Response::CellSize(None)]).unwrap();
        assert_eq!(
            require_reported_cell_size_for_sixel(
                sixel_without_valid_cell_size,
                &capabilities,
                true
            ),
            None
        );

        let (sixel_with_cell_size, _, capabilities) =
            interpret_parser_responses(vec![Response::Sixel, Response::CellSize(Some((10, 20)))])
                .unwrap();
        assert_eq!(
            require_reported_cell_size_for_sixel(sixel_with_cell_size, &capabilities, true),
            Some(ProtocolType::Sixel)
        );
        assert_eq!(
            require_reported_cell_size_for_sixel(sixel_without_cell_size, &[], false),
            Some(ProtocolType::Sixel)
        );
        assert_eq!(
            require_reported_cell_size_for_sixel(Some(ProtocolType::Kitty), &[], true),
            Some(ProtocolType::Kitty)
        );
    }

    #[test]
    fn test_interpret_parser_responses_text_sizing_protocol() {
        let (_, _, caps) = interpret_parser_responses(vec![
            // Example response from Kitty.
            Response::CursorPositionReport(1, 1),
            Response::CursorPositionReport(3, 1),
            Response::CursorPositionReport(5, 1),
        ])
        .unwrap();
        assert!(caps.contains(&Capability::TextSizingProtocol));
    }

    #[test]
    fn test_interpret_parser_responses_text_sizing_protocol_incomplete() {
        let (_, _, caps) = interpret_parser_responses(vec![
            // Example response from Foot, notably moves 2 columns only on `w=2` query, but not
            // `s=2`.
            Response::CursorPositionReport(1, 22),
            Response::CursorPositionReport(3, 22),
            Response::CursorPositionReport(4, 22),
        ])
        .unwrap();
        assert!(!caps.contains(&Capability::TextSizingProtocol));
    }
}
