use std::ffi::OsString;
use std::io;
use std::time::{Duration, Instant};

pub(super) const OWNER_PROBE_TIMEOUT: Duration = Duration::from_millis(500);

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum TerminalOwnerProbe {
    Direct,
    Layers(Vec<TerminalOwnerLayer>),
    Unsupported { reason: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum TerminalOwnerLayer {
    Tmux { pane: OsString },
    Screen { session: OsString },
    Zellij { session: OsString },
}

impl TerminalOwnerProbe {
    pub(super) fn detect_with(mut env: impl FnMut(&str) -> Option<OsString>) -> Self {
        let term = env("TERM")
            .unwrap_or_default()
            .to_string_lossy()
            .to_ascii_lowercase();
        let zellij_marker = env("ZELLIJ");
        let zellij_session = env("ZELLIJ_SESSION_NAME");
        let tmux_marker = env("TMUX");
        let tmux_pane = env("TMUX_PANE");
        let screen_session = env("STY");

        let mut layers = Vec::with_capacity(3);
        let mut incomplete = Vec::new();
        if zellij_marker.is_some() || zellij_session.is_some() {
            match zellij_session {
                Some(session) => layers.push(TerminalOwnerLayer::Zellij { session }),
                None => incomplete.push(
                    "Zellij was detected without ZELLIJ_SESSION_NAME, so client attachment cannot be verified",
                ),
            }
        }
        if tmux_marker.is_some() || tmux_pane.is_some() {
            match tmux_pane {
                Some(pane) => layers.push(TerminalOwnerLayer::Tmux { pane }),
                None => incomplete.push(
                    "tmux was detected without TMUX_PANE, so client attachment cannot be verified",
                ),
            }
        }
        if let Some(session) = screen_session {
            layers.push(TerminalOwnerLayer::Screen { session });
        }

        if !incomplete.is_empty() {
            return Self::Unsupported {
                reason: incomplete.join("; "),
            };
        }
        if !layers.is_empty() {
            // Multiplexer environment variables are inherited through nesting. Every explicit
            // layer must therefore still have a client: checking only the innermost layer is not
            // sufficient because tmux/Zellij synthesize CPR inside their virtual terminal even
            // after an outer client detaches. TERM is deliberately ignored here; tmux commonly
            // advertises `screen-*`, which is not evidence of an additional GNU screen layer.
            return Self::Layers(layers);
        }

        if term.starts_with("tmux") {
            Self::Unsupported {
                reason: "a tmux-compatible terminal was detected without TMUX_PANE, so client attachment cannot be verified"
                    .to_owned(),
            }
        } else if term.starts_with("screen") {
            Self::Unsupported {
                reason: "a screen-compatible multiplexer was detected without STY, so client attachment cannot be verified"
                    .to_owned(),
            }
        } else {
            Self::Direct
        }
    }

    pub(super) fn check_attached(&self) -> io::Result<()> {
        match self {
            Self::Direct => Ok(()),
            Self::Unsupported { reason } => {
                Err(io::Error::new(io::ErrorKind::Unsupported, reason.clone()))
            }
            Self::Layers(_) => self.check_layers_with(
                OWNER_PROBE_TIMEOUT,
                TerminalOwnerLayer::check_attached_until,
            ),
        }
    }

    fn check_layers_with<F>(&self, timeout: Duration, check: F) -> io::Result<()>
    where
        F: Fn(&TerminalOwnerLayer, Instant) -> io::Result<()> + Sync,
    {
        let layers = match self {
            Self::Direct => return Ok(()),
            Self::Unsupported { reason } => {
                return Err(io::Error::new(io::ErrorKind::Unsupported, reason.clone()));
            }
            Self::Layers(layers) => layers,
        };
        let deadline = Instant::now() + timeout;
        if let [layer] = layers.as_slice() {
            return check(layer, deadline);
        }

        // Each CLI owns its own bounded process tree. Running all explicit nesting layers under one
        // shared deadline keeps the total owner-query budget at 500 ms instead of multiplying it by
        // the nesting depth. Iterate the joined results in layer order for deterministic errors.
        std::thread::scope(|scope| {
            let check = &check;
            let handles = layers
                .iter()
                .map(|layer| scope.spawn(move || check(layer, deadline)))
                .collect::<Vec<_>>();
            for handle in handles {
                match handle.join() {
                    Ok(result) => result?,
                    Err(_) => {
                        return Err(io::Error::other(
                            "terminal multiplexer attachment query panicked",
                        ));
                    }
                }
            }
            Ok(())
        })
    }
}

impl TerminalOwnerLayer {
    fn check_attached_until(&self, deadline: Instant) -> io::Result<()> {
        let timeout = deadline
            .checked_duration_since(Instant::now())
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::TimedOut,
                    "terminal multiplexer attachment queries exceeded their shared deadline",
                )
            })?;
        if timeout.is_zero() {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "terminal multiplexer attachment queries exceeded their shared deadline",
            ));
        }

        match self {
            Self::Tmux { pane } => {
                let args = [
                    OsString::from("list-clients"),
                    OsString::from("-t"),
                    pane.clone(),
                    OsString::from("-F"),
                    OsString::from("#{client_control_mode}\t#{client_termname}"),
                ];
                let output = run_command_bounded("tmux", &args, timeout)?;
                if !output.status.success() {
                    return Err(command_failure("tmux attachment query", &output));
                }
                match tmux_has_direct_terminal_client(&output.stdout) {
                    Some(true) => Ok(()),
                    Some(false) => Err(io::Error::new(
                        io::ErrorKind::BrokenPipe,
                        "the tmux session has no independently attributable non-control direct terminal client",
                    )),
                    None => Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "tmux returned an invalid client listing",
                    )),
                }
            }
            Self::Screen { session } => {
                let args = [OsString::from("-ls"), session.clone()];
                let output = run_command_bounded("screen", &args, timeout)?;
                if !output.status.success() {
                    return Err(command_failure("GNU screen attachment query", &output));
                }
                let mut listing = output.stdout;
                listing.extend_from_slice(&output.stderr);
                match screen_session_attached(&listing, session) {
                    Some(true) => Ok(()),
                    Some(false) => Err(io::Error::new(
                        io::ErrorKind::BrokenPipe,
                        "the GNU screen session has no attached clients",
                    )),
                    None => Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "GNU screen did not report an attachment state for the current session",
                    )),
                }
            }
            Self::Zellij { session } => {
                let args = [
                    OsString::from("--session"),
                    session.clone(),
                    OsString::from("action"),
                    OsString::from("list-clients"),
                ];
                let output = run_command_bounded("zellij", &args, timeout)?;
                if !output.status.success() {
                    return Err(command_failure(
                        "Zellij client query (requires Zellij 0.40.1 or newer)",
                        &output,
                    ));
                }
                match zellij_has_attached_client(&output.stdout) {
                    Some(true) => Ok(()),
                    Some(false) => Err(io::Error::new(
                        io::ErrorKind::BrokenPipe,
                        "the Zellij session has no attached clients",
                    )),
                    None => Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "Zellij returned an invalid client listing",
                    )),
                }
            }
        }
    }
}

struct CommandOutput {
    status: std::process::ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

fn run_command_bounded(
    program: &str,
    args: &[OsString],
    timeout: Duration,
) -> io::Result<CommandOutput> {
    use std::process::{Command, Stdio};

    // Reuse the bounded concurrent drain/tree-reap primitive. Waiting for exit before draining
    // lets a verbose or hostile PATH replacement fill its pipe forever; killing only the direct
    // child also lets an inherited pipe in a descendant pin this sole liveness worker. The
    // isolated YtDlp profile supplies the same Unix process-group/Windows Job semantics without
    // applying any tool-specific environment to this raw Command.
    let mut command = Command::new(program);
    command.args(args).stdin(Stdio::null());
    let output = crate::util::process::std_output_limited(
        command,
        crate::util::process::ProcessProfile::YtDlp,
        timeout,
        64 * 1024,
    )
    .map_err(|error| {
        io::Error::other(format!(
            "{program} attachment query failed or exceeded its bound: {error:#}"
        ))
    })?;
    Ok(CommandOutput {
        status: output.status,
        stdout: output.stdout,
        stderr: output.stderr_tail,
    })
}

fn command_failure(label: &str, output: &CommandOutput) -> io::Error {
    let detail = if output.stderr.is_empty() {
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    } else {
        String::from_utf8_lossy(&output.stderr).trim().to_owned()
    };
    io::Error::other(if detail.is_empty() {
        format!("{label} failed with {}", output.status)
    } else {
        format!("{label} failed: {detail}")
    })
}

fn screen_session_attached(listing: &[u8], session: &OsString) -> Option<bool> {
    let session = session.to_string_lossy();
    String::from_utf8_lossy(listing).lines().find_map(|line| {
        if !line.contains(session.as_ref()) {
            return None;
        }
        let status = line
            .rsplit_once('(')
            .and_then(|(_, tail)| tail.split_once(')'))
            .map(|(status, _)| status.trim().to_ascii_lowercase())?;
        if status.contains("detached") {
            Some(false)
        } else if status.contains("attached") || status.contains("multi") {
            Some(true)
        } else {
            None
        }
    })
}

fn tmux_has_direct_terminal_client(listing: &[u8]) -> Option<bool> {
    let listing = std::str::from_utf8(listing).ok()?;
    let mut has_direct_client = false;
    for line in listing.lines().filter(|line| !line.trim().is_empty()) {
        let mut fields = line.split('\t');
        let control_mode = fields.next()?;
        let termname = fields.next()?;
        if fields.next().is_some() {
            return None;
        }
        let control_mode = match control_mode.trim() {
            "0" => false,
            "1" => true,
            _ => return None,
        };
        let termname = termname.trim().to_ascii_lowercase();
        let nested_multiplexer = termname.starts_with("tmux") || termname.starts_with("screen");
        if !control_mode && !termname.is_empty() && !nested_multiplexer {
            has_direct_client = true;
        }
    }
    Some(has_direct_client)
}

fn zellij_has_attached_client(listing: &[u8]) -> Option<bool> {
    let listing = String::from_utf8_lossy(listing);
    let mut lines = listing
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty());
    let header = lines.next()?;
    if !header.to_ascii_uppercase().starts_with("CLIENT_ID") {
        return None;
    }
    Some(lines.next().is_some())
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::{Arc, Condvar, Mutex};

    use super::*;

    #[test]
    fn multiplexer_detection_is_fail_closed_when_identity_is_incomplete() {
        let env = HashMap::from([
            ("TERM", OsString::from("screen-256color")),
            ("TMUX", OsString::from("/tmp/tmux.sock,1,0")),
        ]);
        let detected = TerminalOwnerProbe::detect_with(|key| env.get(key).cloned());
        assert!(matches!(detected, TerminalOwnerProbe::Unsupported { .. }));

        let env = HashMap::from([("ZELLIJ", OsString::from("0"))]);
        let detected = TerminalOwnerProbe::detect_with(|key| env.get(key).cloned());
        assert!(matches!(detected, TerminalOwnerProbe::Unsupported { .. }));
    }

    #[test]
    fn explicit_tmux_identity_does_not_invent_a_screen_layer_from_term() {
        let env = HashMap::from([
            ("TERM", OsString::from("screen-256color")),
            ("TMUX", OsString::from("/tmp/tmux.sock,1,0")),
            ("TMUX_PANE", OsString::from("%4")),
        ]);
        assert_eq!(
            TerminalOwnerProbe::detect_with(|key| env.get(key).cloned()),
            TerminalOwnerProbe::Layers(vec![TerminalOwnerLayer::Tmux {
                pane: OsString::from("%4")
            }])
        );
    }

    #[test]
    fn every_explicit_nested_multiplexer_layer_is_retained() {
        let env = HashMap::from([
            ("TERM", OsString::from("screen-256color")),
            ("ZELLIJ", OsString::from("0")),
            ("ZELLIJ_SESSION_NAME", OsString::from("outer-zellij")),
            ("TMUX", OsString::from("/tmp/tmux.sock,1,0")),
            ("TMUX_PANE", OsString::from("%4")),
            ("STY", OsString::from("123.inner-screen")),
        ]);
        assert_eq!(
            TerminalOwnerProbe::detect_with(|key| env.get(key).cloned()),
            TerminalOwnerProbe::Layers(vec![
                TerminalOwnerLayer::Zellij {
                    session: OsString::from("outer-zellij"),
                },
                TerminalOwnerLayer::Tmux {
                    pane: OsString::from("%4"),
                },
                TerminalOwnerLayer::Screen {
                    session: OsString::from("123.inner-screen"),
                },
            ])
        );
    }

    #[test]
    fn term_only_multiplexer_identity_remains_fail_closed() {
        for term in ["tmux-256color", "screen-256color"] {
            let env = HashMap::from([("TERM", OsString::from(term))]);
            assert!(matches!(
                TerminalOwnerProbe::detect_with(|key| env.get(key).cloned()),
                TerminalOwnerProbe::Unsupported { .. }
            ));
        }
    }

    #[test]
    fn nested_multiplexer_queries_run_in_parallel_under_one_deadline() {
        let probe = TerminalOwnerProbe::Layers(vec![
            TerminalOwnerLayer::Zellij {
                session: OsString::from("zellij"),
            },
            TerminalOwnerLayer::Tmux {
                pane: OsString::from("%1"),
            },
            TerminalOwnerLayer::Screen {
                session: OsString::from("123.screen"),
            },
        ]);
        let rendezvous = Arc::new((Mutex::new(0usize), Condvar::new()));
        let rendezvous_check = Arc::clone(&rendezvous);

        probe
            .check_layers_with(Duration::from_millis(250), move |_layer, deadline| {
                let (entered, changed) = &*rendezvous_check;
                let mut count = entered
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                *count += 1;
                changed.notify_all();
                while *count < 3 {
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    if remaining.is_zero() {
                        return Err(io::Error::new(
                            io::ErrorKind::TimedOut,
                            "nested attachment checks did not overlap",
                        ));
                    }
                    let waited = changed.wait_timeout(count, remaining);
                    let (next, timed) = waited.unwrap_or_else(std::sync::PoisonError::into_inner);
                    count = next;
                    if timed.timed_out() && *count < 3 {
                        return Err(io::Error::new(
                            io::ErrorKind::TimedOut,
                            "nested attachment checks did not overlap",
                        ));
                    }
                }
                Ok(())
            })
            .expect("all explicit multiplexer checks must overlap");
        assert_eq!(*rendezvous.0.lock().unwrap(), 3);
    }

    #[test]
    fn screen_listing_distinguishes_attached_and_detached() {
        let session = OsString::from("123.music");
        assert_eq!(
            screen_session_attached(b"\t123.music\t(Attached)\n", &session),
            Some(true)
        );
        assert_eq!(
            screen_session_attached(b"\t123.music\t(Detached)\n", &session),
            Some(false)
        );
        assert_eq!(
            screen_session_attached(b"\t123.music\t(Multi, attached)\n", &session),
            Some(true)
        );
    }

    #[test]
    fn tmux_client_listing_requires_a_non_control_direct_terminal() {
        assert_eq!(
            tmux_has_direct_terminal_client(b"0\txterm-256color\n"),
            Some(true)
        );
        assert_eq!(
            tmux_has_direct_terminal_client(b"1\txterm-256color\n0\tlinux\n"),
            Some(true)
        );
        assert_eq!(tmux_has_direct_terminal_client(b""), Some(false));
        assert_eq!(
            tmux_has_direct_terminal_client(b"1\txterm-256color\n"),
            Some(false)
        );
        assert_eq!(
            tmux_has_direct_terminal_client(b"0\ttmux-256color\n"),
            Some(false)
        );
        assert_eq!(
            tmux_has_direct_terminal_client(b"0\tscreen-256color\n"),
            Some(false)
        );
        assert_eq!(tmux_has_direct_terminal_client(b"0\t\n"), Some(false));
        assert_eq!(tmux_has_direct_terminal_client(b"no-delimiter\n"), None);
        assert_eq!(tmux_has_direct_terminal_client(b"yes\txterm\n"), None);
        assert_eq!(
            tmux_has_direct_terminal_client(b"0\txterm\n0\txterm\textra\n"),
            None
        );
        assert_eq!(tmux_has_direct_terminal_client(b"\xff\n"), None);
    }

    #[test]
    fn zellij_client_listing_requires_header_and_a_client_row() {
        assert_eq!(
            zellij_has_attached_client(b"CLIENT_ID ZELLIJ_PANE_ID RUNNING_COMMAND\n1 2 ytt\n"),
            Some(true)
        );
        assert_eq!(
            zellij_has_attached_client(b"CLIENT_ID ZELLIJ_PANE_ID RUNNING_COMMAND\n"),
            Some(false)
        );
        assert_eq!(zellij_has_attached_client(b"old zellij output\n"), None);
    }
}
