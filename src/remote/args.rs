//! Hand-rolled parser for `ytt -r <command> [args] [flags]`.
//!
//! The project ships no `clap`; this mirrors the existing `--version`/`--help` style in
//! `main`. The verb (with short aliases) maps to a [`RemoteCommand`]; `-q`/`--json` are
//! client-side display flags.

use super::proto::{RemoteCommand, ToggleState};

/// A successfully parsed `ytt -r` invocation.
pub struct Parsed {
    pub command: RemoteCommand,
    /// Suppress the success line (errors still print).
    pub quiet: bool,
    /// Print the raw JSON response instead of the human line.
    pub json: bool,
}

/// Why parsing stopped.
pub enum ParseError {
    /// `-h`/`--help` or no command — print usage to stdout, exit 0.
    Usage(String),
    /// Unknown verb / bad argument — print to stderr, exit 2.
    Invalid(String),
}

pub const USAGE: &str = "\
Usage: ytt -r <command> [flags]

Control a running ytt instance over its local control socket.

Commands:
  next, n                 Skip to the next track
  prev, p                 Go to the previous track
  play <query>            Search and play the first result (daemon)
  enqueue <query>         Search and add the first result (daemon)
  play-pause, pp, toggle  Toggle play / pause
  up, vol-up              Volume up
  down, vol-down          Volume down
  volume <0-100>          Set the volume to an absolute percent
  back                    Seek backward
  fwd, forward            Seek forward
  seek-to <seconds>       Seek to an absolute position in the current track
  streaming [on|off|toggle]
                          Toggle (or set) autoplay streaming
  resume-session          Load and play the saved session
  status, st              Print the current track / state
  quit                    Quit the running instance

Flags:
  -q, --quiet             Suppress the success line (errors still print)
      --json              Print the raw JSON response
  -h, --help              Show this help

Examples:
  ytt -r pp               # play / pause
  ytt -r streaming off    # turn autoplay streaming off
  bindsym XF86AudioNext exec ytt -r next   # i3 / sway media key
";

/// Parse the arguments that follow `ytt -r` (i.e. `std::env::args().skip(2)`).
pub fn parse(args: &[String]) -> Result<Parsed, ParseError> {
    let mut verb: Option<&str> = None;
    let mut rest: Vec<&str> = Vec::new();
    let mut quiet = false;
    let mut json = false;

    for a in args {
        match a.as_str() {
            "-h" | "--help" => return Err(ParseError::Usage(USAGE.to_string())),
            "-q" | "--quiet" => quiet = true,
            "--json" => json = true,
            other if verb.is_none() => verb = Some(other),
            other => rest.push(other),
        }
    }

    let Some(verb) = verb else {
        return Err(ParseError::Usage(USAGE.to_string()));
    };

    let command = match verb {
        "next" | "n" => RemoteCommand::Next,
        "prev" | "p" | "previous" => RemoteCommand::Prev,
        "play" if !rest.is_empty() => RemoteCommand::Play {
            query: rest.join(" "),
        },
        "enqueue" | "queue" | "add" => {
            if rest.is_empty() {
                return Err(ParseError::Invalid(format!(
                    "{verb}: expected a search query"
                )));
            }
            RemoteCommand::Enqueue {
                query: rest.join(" "),
            }
        }
        "play-pause" | "pp" | "toggle" | "play" | "pause" => RemoteCommand::TogglePause,
        "up" | "vol-up" | "volup" => RemoteCommand::VolumeUp,
        "down" | "vol-down" | "voldown" => RemoteCommand::VolumeDown,
        "volume" | "vol" => {
            let percent = rest
                .first()
                .and_then(|value| value.parse::<i64>().ok())
                .filter(|value| (0..=100).contains(value));
            match percent {
                Some(percent) => RemoteCommand::SetVolume { percent },
                None => {
                    return Err(ParseError::Invalid(format!(
                        "{verb}: expected a percent between 0 and 100"
                    )));
                }
            }
        }
        "back" | "rewind" => RemoteCommand::SeekBack,
        "fwd" | "forward" | "ff" => RemoteCommand::SeekForward,
        "seek-to" | "seekto" => {
            let seconds = rest.first().and_then(|value| value.parse::<f64>().ok());
            match seconds {
                Some(seconds) if seconds >= 0.0 && seconds.is_finite() => RemoteCommand::SeekTo {
                    ms: (seconds * 1000.0).round() as u64,
                },
                _ => {
                    return Err(ParseError::Invalid(format!(
                        "{verb}: expected a non-negative position in seconds"
                    )));
                }
            }
        }
        "streaming" | "radio" => {
            let state = match rest.first().copied() {
                None => ToggleState::Toggle,
                Some("on" | "true" | "1") => ToggleState::On,
                Some("off" | "false" | "0") => ToggleState::Off,
                Some("toggle") => ToggleState::Toggle,
                Some(other) => {
                    return Err(ParseError::Invalid(format!(
                        "{verb}: expected on|off|toggle, got `{other}`"
                    )));
                }
            };
            RemoteCommand::Streaming { state }
        }
        "resume-session" | "load-session" => RemoteCommand::ResumeSession,
        "status" | "st" => RemoteCommand::Status,
        "quit" | "exit" => RemoteCommand::Quit,
        other => {
            return Err(ParseError::Invalid(format!(
                "unknown command `{other}` (try `ytt -r --help`)"
            )));
        }
    };

    Ok(Parsed {
        command,
        quiet,
        json,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cmd(args: &[&str]) -> RemoteCommand {
        let owned: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        match parse(&owned) {
            Ok(p) => p.command,
            Err(ParseError::Invalid(m)) => panic!("unexpected invalid: {m}"),
            Err(ParseError::Usage(_)) => panic!("unexpected usage"),
        }
    }

    #[test]
    fn aliases_map_to_commands() {
        assert_eq!(cmd(&["n"]), RemoteCommand::Next);
        assert_eq!(cmd(&["next"]), RemoteCommand::Next);
        assert_eq!(cmd(&["p"]), RemoteCommand::Prev);
        assert_eq!(cmd(&["pp"]), RemoteCommand::TogglePause);
        assert_eq!(cmd(&["toggle"]), RemoteCommand::TogglePause);
        assert_eq!(
            cmd(&["play", "new", "song"]),
            RemoteCommand::Play {
                query: "new song".to_string()
            }
        );
        assert_eq!(
            cmd(&["enqueue", "new", "song"]),
            RemoteCommand::Enqueue {
                query: "new song".to_string()
            }
        );
        assert_eq!(cmd(&["up"]), RemoteCommand::VolumeUp);
        assert_eq!(cmd(&["vol-down"]), RemoteCommand::VolumeDown);
        assert_eq!(
            cmd(&["volume", "55"]),
            RemoteCommand::SetVolume { percent: 55 }
        );
        assert_eq!(cmd(&["back"]), RemoteCommand::SeekBack);
        assert_eq!(cmd(&["fwd"]), RemoteCommand::SeekForward);
        assert_eq!(
            cmd(&["seek-to", "92.5"]),
            RemoteCommand::SeekTo { ms: 92_500 }
        );
        assert_eq!(cmd(&["resume-session"]), RemoteCommand::ResumeSession);
        assert_eq!(cmd(&["load-session"]), RemoteCommand::ResumeSession);
        assert_eq!(cmd(&["status"]), RemoteCommand::Status);
        assert_eq!(cmd(&["quit"]), RemoteCommand::Quit);
    }

    #[test]
    fn streaming_states() {
        assert_eq!(
            cmd(&["streaming"]),
            RemoteCommand::Streaming {
                state: ToggleState::Toggle
            }
        );
        assert_eq!(
            cmd(&["streaming", "on"]),
            RemoteCommand::Streaming {
                state: ToggleState::On
            }
        );
        assert_eq!(
            cmd(&["streaming", "off"]),
            RemoteCommand::Streaming {
                state: ToggleState::Off
            }
        );
    }

    #[test]
    fn legacy_radio_alias_maps_to_streaming() {
        assert_eq!(
            cmd(&["radio", "on"]),
            RemoteCommand::Streaming {
                state: ToggleState::On
            }
        );
    }

    #[test]
    fn streaming_bad_state_is_invalid() {
        let owned = vec!["streaming".to_string(), "loud".to_string()];
        assert!(matches!(parse(&owned), Err(ParseError::Invalid(_))));
    }

    #[test]
    fn enqueue_requires_query() {
        let owned = vec!["enqueue".to_string()];
        assert!(matches!(parse(&owned), Err(ParseError::Invalid(_))));
    }

    #[test]
    fn volume_and_seek_reject_bad_values() {
        for args in [
            &["volume"][..],
            &["volume", "150"][..],
            &["volume", "loud"][..],
            &["seek-to"][..],
            &["seek-to", "-3"][..],
        ] {
            let owned: Vec<String> = args.iter().map(|s| s.to_string()).collect();
            assert!(
                matches!(parse(&owned), Err(ParseError::Invalid(_))),
                "{args:?}"
            );
        }
    }

    #[test]
    fn unknown_verb_is_invalid() {
        let owned = vec!["frobnicate".to_string()];
        assert!(matches!(parse(&owned), Err(ParseError::Invalid(_))));
    }

    #[test]
    fn empty_and_help_are_usage() {
        assert!(matches!(parse(&[]), Err(ParseError::Usage(_))));
        assert!(matches!(
            parse(&["--help".to_string()]),
            Err(ParseError::Usage(_))
        ));
    }

    #[test]
    fn flags_parse_in_any_position() {
        let owned: Vec<String> = ["-q", "next", "--json"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let p = parse(&owned).unwrap_or_else(|_| panic!("should parse"));
        assert_eq!(p.command, RemoteCommand::Next);
        assert!(p.quiet);
        assert!(p.json);
    }
}
