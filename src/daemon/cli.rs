use super::DaemonCommand;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum ParseOutcome {
    Usage,
    Invalid(String),
}

pub(super) fn parse(args: &[String]) -> Result<DaemonCommand, ParseOutcome> {
    let Some((verb, rest)) = args.split_first() else {
        return Err(ParseOutcome::Usage);
    };
    if matches!(verb.as_str(), "-h" | "--help") {
        return Err(ParseOutcome::Usage);
    }

    match verb.as_str() {
        "start" => {
            let mut resume = false;
            for arg in rest {
                match arg.as_str() {
                    "--resume" => resume = true,
                    "-h" | "--help" => return Err(ParseOutcome::Usage),
                    other => {
                        return Err(ParseOutcome::Invalid(format!(
                            "start: unknown flag `{other}`"
                        )));
                    }
                }
            }
            Ok(DaemonCommand::Start { resume })
        }
        "serve" => {
            let mut from_tray = false;
            let mut resume = false;
            for arg in rest {
                match arg.as_str() {
                    "--from-tray" => from_tray = true,
                    "--resume" => resume = true,
                    "-h" | "--help" => return Err(ParseOutcome::Usage),
                    other => {
                        return Err(ParseOutcome::Invalid(format!(
                            "serve: unknown flag `{other}`"
                        )));
                    }
                }
            }
            Ok(DaemonCommand::Serve { from_tray, resume })
        }
        "status" => {
            let mut json = false;
            for arg in rest {
                match arg.as_str() {
                    "--json" => json = true,
                    "-h" | "--help" => return Err(ParseOutcome::Usage),
                    other => {
                        return Err(ParseOutcome::Invalid(format!(
                            "status: unknown flag `{other}`"
                        )));
                    }
                }
            }
            Ok(DaemonCommand::Status { json })
        }
        "stop" => {
            if !rest.is_empty() {
                return Err(ParseOutcome::Invalid(
                    "stop: unexpected arguments".to_string(),
                ));
            }
            Ok(DaemonCommand::Stop)
        }
        other => Err(ParseOutcome::Invalid(format!(
            "unknown command `{other}` (try `ytt daemon --help`)"
        ))),
    }
}
