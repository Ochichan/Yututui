use std::ffi::OsString;
use std::io;
use std::time::{Duration, Instant};

pub(super) use crate::terminal_policy::OWNER_PROBE_TIMEOUT;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum OwnerProbeDomain {
    Environment,
    Tmux,
    Screen,
    Zellij,
}

#[derive(Debug)]
pub(super) enum OwnerProbeCheck {
    /// A direct terminal has no independent owner CLI. This is intentionally not treated as an
    /// `Alive` observation: only CPR or real input can clear transport suspicion.
    Direct,
    Layers(Vec<OwnerLayerCheck>),
}

#[derive(Debug)]
pub(super) struct OwnerLayerCheck {
    pub(super) domain: OwnerProbeDomain,
    pub(super) result: io::Result<()>,
}

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
        let zellij_session = env("ZELLIJ_SESSION_NAME").filter(|value| !value.is_empty());
        let tmux_marker = env("TMUX");
        let tmux_pane = env("TMUX_PANE").filter(|value| !value.is_empty());
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
        match screen_session {
            Some(session) if !session.is_empty() => {
                layers.push(TerminalOwnerLayer::Screen { session });
            }
            Some(_) => incomplete.push(
                "GNU screen was detected without a non-empty STY, so client attachment cannot be verified",
            ),
            None => {}
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

    /// Return every independently attributable layer result under one shared command budget.
    /// Liveness policy is intentionally left to the caller so one operational CLI failure cannot
    /// be mistaken for proof that a terminal client disappeared.
    pub(super) fn check(&self) -> OwnerProbeCheck {
        match self {
            Self::Direct => OwnerProbeCheck::Direct,
            Self::Unsupported { reason } => OwnerProbeCheck::Layers(vec![OwnerLayerCheck {
                domain: OwnerProbeDomain::Environment,
                result: Err(io::Error::new(io::ErrorKind::Unsupported, reason.clone())),
            }]),
            Self::Layers(layers) => {
                let results = self.collect_layers_with(
                    OWNER_PROBE_TIMEOUT,
                    TerminalOwnerLayer::check_attached_until,
                );
                OwnerProbeCheck::Layers(
                    layers
                        .iter()
                        .zip(results)
                        .map(|(layer, result)| OwnerLayerCheck {
                            domain: layer.domain(),
                            result,
                        })
                        .collect(),
                )
            }
        }
    }

    #[cfg(test)]
    fn check_layers_with<F>(&self, timeout: Duration, check: F) -> io::Result<()>
    where
        F: Fn(&TerminalOwnerLayer, Instant) -> io::Result<()> + Sync,
    {
        match self {
            Self::Direct => return Ok(()),
            Self::Unsupported { reason } => {
                return Err(io::Error::new(io::ErrorKind::Unsupported, reason.clone()));
            }
            Self::Layers(_) => {}
        }
        for result in self.collect_layers_with(timeout, check) {
            result?;
        }
        Ok(())
    }

    fn collect_layers_with<F>(&self, timeout: Duration, check: F) -> Vec<io::Result<()>>
    where
        F: Fn(&TerminalOwnerLayer, Instant) -> io::Result<()> + Sync,
    {
        let layers = match self {
            Self::Layers(layers) => layers,
            _ => return Vec::new(),
        };
        let deadline = Instant::now() + timeout;
        if let [layer] = layers.as_slice() {
            return vec![check(layer, deadline)];
        }

        // Each CLI owns its own bounded process tree. Running all explicit nesting layers under one
        // shared deadline keeps the total owner-query budget at 500 ms instead of multiplying it by
        // the nesting depth. Preserve layer order so each result keeps a stable evidence domain.
        std::thread::scope(|scope| {
            let check = &check;
            layers
                .iter()
                .map(|layer| scope.spawn(move || check(layer, deadline)))
                .collect::<Vec<_>>()
                .into_iter()
                .map(|handle| {
                    handle.join().unwrap_or_else(|_| {
                        Err(io::Error::other(
                            "terminal multiplexer attachment query panicked",
                        ))
                    })
                })
                .collect()
        })
    }
}

impl TerminalOwnerLayer {
    fn domain(&self) -> OwnerProbeDomain {
        match self {
            Self::Tmux { .. } => OwnerProbeDomain::Tmux,
            Self::Screen { .. } => OwnerProbeDomain::Screen,
            Self::Zellij { .. } => OwnerProbeDomain::Zellij,
        }
    }

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
                match tmux_has_attached_client(&output.stdout) {
                    Some(true) => Ok(()),
                    Some(false) => Err(io::Error::new(
                        io::ErrorKind::BrokenPipe,
                        "the tmux session has no attached terminal clients",
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
    .map_err(|_error| {
        io::Error::other(format!(
            "{program} attachment query failed or exceeded its bound"
        ))
    })?;
    Ok(CommandOutput {
        status: output.status,
        stdout: output.stdout,
        stderr: output.stderr_tail,
    })
}

fn command_failure(label: &str, output: &CommandOutput) -> io::Error {
    // CLI output may contain pane/session identifiers, socket paths, or wrapper diagnostics.
    // Keep it available only for attachment-state parsing on successful commands; failures expose
    // the fixed layer label and exit status, never raw stdout/stderr.
    io::Error::other(format!("{label} failed with {}", output.status))
}

fn screen_session_attached(listing: &[u8], session: &OsString) -> Option<bool> {
    let session = session.to_string_lossy();
    String::from_utf8_lossy(listing).lines().find_map(|line| {
        // `screen -ls <match>` may print more than one matching socket. Compare the reported
        // socket token exactly: a substring match can attribute `9123.music (Detached)` to the
        // current `123.music` session and falsely declare a live owner dead.
        if line.split_whitespace().next()? != session.as_ref() {
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

fn tmux_has_attached_client(listing: &[u8]) -> Option<bool> {
    let listing = std::str::from_utf8(listing).ok()?;
    let mut has_attached_client = false;
    for line in listing.lines().filter(|line| !line.trim().is_empty()) {
        let mut fields = line.split('\t');
        let control_mode = fields.next()?;
        let termname = fields.next()?;
        if fields.next().is_some() {
            return None;
        }
        match control_mode.trim() {
            "0" | "1" => {}
            _ => return None,
        }
        // A real client nested inside another tmux legitimately reports tmux-256color (or a
        // screen-derived TERM). It is still attached to this layer. Same-type outer detach cannot
        // be inferred from this public listing and is documented as an owner-probe limitation.
        // Control-mode rows can be the user's visible iTerm2/tmux integration. tmux does not
        // expose a reliable field that distinguishes it from an opaque retained broker, so any
        // well-formed terminal client is attachment evidence and the limitation is documented.
        if !termname.trim().is_empty() {
            has_attached_client = true;
        }
    }
    Some(has_attached_client)
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

        for env in [
            HashMap::from([
                ("TMUX", OsString::from("/tmp/tmux.sock,1,0")),
                ("TMUX_PANE", OsString::new()),
            ]),
            HashMap::from([
                ("ZELLIJ", OsString::from("0")),
                ("ZELLIJ_SESSION_NAME", OsString::new()),
            ]),
            HashMap::from([("STY", OsString::new())]),
        ] {
            assert!(matches!(
                TerminalOwnerProbe::detect_with(|key| env.get(key).cloned()),
                TerminalOwnerProbe::Unsupported { .. }
            ));
        }
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
        assert_eq!(
            screen_session_attached(
                b"\t9123.music\t(Detached)\n\t123.music\t(Attached)\n",
                &session,
            ),
            Some(true),
            "a near-collision session must not be attributed to the current owner",
        );
        assert_eq!(
            screen_session_attached(b"\t9123.music\t(Detached)\n", &session),
            None,
        );
    }

    #[test]
    fn tmux_client_listing_accepts_nested_and_control_mode_terminals() {
        assert_eq!(tmux_has_attached_client(b"0\txterm-256color\n"), Some(true));
        assert_eq!(
            tmux_has_attached_client(b"1\txterm-256color\n0\tlinux\n"),
            Some(true)
        );
        assert_eq!(tmux_has_attached_client(b""), Some(false));
        assert_eq!(tmux_has_attached_client(b"1\txterm-256color\n"), Some(true));
        assert_eq!(tmux_has_attached_client(b"0\ttmux-256color\n"), Some(true));
        assert_eq!(
            tmux_has_attached_client(b"0\tscreen-256color\n"),
            Some(true)
        );
        assert_eq!(tmux_has_attached_client(b"0\t\n"), Some(false));
        assert_eq!(tmux_has_attached_client(b"no-delimiter\n"), None);
        assert_eq!(tmux_has_attached_client(b"yes\txterm\n"), None);
        assert_eq!(
            tmux_has_attached_client(b"0\txterm\n0\txterm\textra\n"),
            None
        );
        assert_eq!(tmux_has_attached_client(b"\xff\n"), None);
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

    #[cfg(unix)]
    #[test]
    fn failed_owner_command_does_not_expose_raw_output() {
        use std::os::unix::process::ExitStatusExt;

        let marker = "private-pane-%7 /private/socket/path";
        let output = CommandOutput {
            status: std::process::ExitStatus::from_raw(9 << 8),
            stdout: marker.as_bytes().to_vec(),
            stderr: marker.as_bytes().to_vec(),
        };
        let message = command_failure("tmux attachment query", &output).to_string();
        assert!(message.contains("tmux attachment query"));
        assert!(message.contains('9'));
        assert!(!message.contains(marker));
        assert!(!message.contains("%7"));
        assert!(!message.contains("/private"));
    }
}
