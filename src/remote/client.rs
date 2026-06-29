//! The `ytt -r <command>` client: a short-lived process that connects to the running
//! instance, sends one command, prints the result, and exits.
//!
//! Critically, this path NEVER touches terminal raw mode or the alternate screen (no
//! `tui::init`, no graphics probe) — it must leave the caller's terminal pristine so it's
//! safe to wire to a window-manager keybinding or a status-bar click.
//!
//! Exit codes follow the i3-msg / swaymsg convention:
//!   0 = applied, 1 = transport / no running instance, 2 = usage or semantic rejection.

use std::time::Duration;

use interprocess::local_socket::GenericFilePath;
use interprocess::local_socket::tokio::Stream;
use interprocess::local_socket::tokio::prelude::*;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::time::timeout;

use super::args::{self, ParseError, Parsed};
use super::endpoint;
use super::proto::{PROTOCOL_VERSION, RemoteRequest, RemoteResponse};

const CONNECT_TIMEOUT: Duration = Duration::from_millis(500);
const REPLY_TIMEOUT: Duration = Duration::from_secs(2);

const EXIT_OK: i32 = 0;
const EXIT_TRANSPORT: i32 = 1;
const EXIT_USAGE: i32 = 2;

/// Entry point from `main` for `ytt -r …`. Parses args, runs the exchange on a tiny
/// current-thread runtime, and returns the process exit code. Never returns to the normal
/// TUI startup path.
pub fn run(args_in: &[String]) -> i32 {
    let parsed = match args::parse(args_in) {
        Ok(p) => p,
        Err(ParseError::Usage(text)) => {
            print!("{text}");
            return EXIT_OK;
        }
        Err(ParseError::Invalid(msg)) => {
            eprintln!("ytt -r: {msg}");
            return EXIT_USAGE;
        }
    };

    let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("ytt -r: could not start runtime: {e}");
            return EXIT_TRANSPORT;
        }
    };
    rt.block_on(exchange(parsed))
}

async fn exchange(parsed: Parsed) -> i32 {
    let Some(instance) = endpoint::read_instance() else {
        eprintln!("ytt -r: no running ytt instance found — start one with `ytt`.");
        return EXIT_TRANSPORT;
    };
    let Ok(name) = instance.endpoint.as_str().to_fs_name::<GenericFilePath>() else {
        eprintln!("ytt -r: malformed endpoint in the instance descriptor.");
        return EXIT_TRANSPORT;
    };

    let conn = match timeout(CONNECT_TIMEOUT, Stream::connect(name)).await {
        Ok(Ok(c)) => c,
        // Connect refused / timed out: the descriptor is stale or the instance just exited.
        _ => {
            eprintln!("ytt -r: could not reach ytt (it may have exited) — start one with `ytt`.");
            return EXIT_TRANSPORT;
        }
    };

    let req = RemoteRequest {
        version: PROTOCOL_VERSION,
        token: instance.token,
        command: parsed.command,
    };
    let mut payload = match serde_json::to_vec(&req) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("ytt -r: could not encode request: {e}");
            return EXIT_TRANSPORT;
        }
    };
    payload.push(b'\n');

    {
        let mut writer = &conn;
        if let Err(e) = writer.write_all(&payload).await {
            eprintln!("ytt -r: write failed: {e}");
            return EXIT_TRANSPORT;
        }
        if let Err(e) = writer.flush().await {
            eprintln!("ytt -r: flush failed: {e}");
            return EXIT_TRANSPORT;
        }
    }

    let mut reader = BufReader::new(&conn);
    let mut line = String::new();
    let resp: RemoteResponse = match timeout(REPLY_TIMEOUT, reader.read_line(&mut line)).await {
        Ok(Ok(n)) if n > 0 => match serde_json::from_str(line.trim()) {
            Ok(r) => r,
            Err(_) => {
                eprintln!("ytt -r: malformed response from ytt.");
                return EXIT_TRANSPORT;
            }
        },
        _ => {
            eprintln!("ytt -r: no response from ytt.");
            return EXIT_TRANSPORT;
        }
    };

    if resp.ok {
        if parsed.json {
            println!("{}", line.trim());
        } else if !parsed.quiet
            && let Some(msg) = &resp.message
        {
            println!("{msg}");
        }
        EXIT_OK
    } else {
        // Errors always print, even under `-q`. The machine reason is the actionable bit.
        let reason = resp.reason.as_deref().unwrap_or("rejected");
        eprintln!("ytt -r: {reason}");
        EXIT_USAGE
    }
}
