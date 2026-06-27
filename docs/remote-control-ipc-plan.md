# Remote Control IPC Investigation

Branch: `investigate/remote-control-ipc`

Scope: investigation and implementation plan only. No feature code has been implemented on this
branch.

## Source Notes

- mpv already supports JSON IPC through `--input-ipc-server`; this project uses that only as the
  private control channel between `ytt` and its owned mpv child.
  Reference: https://mpv.io/manual/master/#json-ipc
- The existing Rust dependency `interprocess` provides cross-platform local sockets backed by Unix
  domain sockets on Unix and named pipes on Windows. Its local socket API does not add framing, so
  newline-delimited JSON is still an application protocol decision.
  Reference: https://docs.rs/interprocess/latest/interprocess/local_socket/
- Future desktop media integrations are possible but platform-specific:
  MPRIS on Linux exposes `Next`, `Previous`, and playback controls over D-Bus.
  Reference: https://specifications.freedesktop.org/mpris-spec/latest/Player_Interface.html
- Windows exposes media transport controls through `SystemMediaTransportControls`.
  Reference: https://learn.microsoft.com/en-us/uwp/api/windows.media.systemmediatransportcontrols
- macOS exposes remote media commands through `MPRemoteCommandCenter`.
  Reference: https://developer.apple.com/documentation/mediaplayer/mpremotecommandcenter

## Current Architecture Findings

- The shipped command is `ytt`, not `ytm`. The concept can be `ytt n` immediately; a literal
  `ytm n` would require a rename, second binary alias, shell alias, or package-manager alias.
- `src/main.rs` currently initializes the terminal before entering the main run loop. Remote CLI
  commands must be parsed before `tui::init()` so `ytt n` does not enter raw mode or flicker the
  terminal.
- `src/main.rs` has a single event pump that selects between crossterm events and `worker_rx`.
  Remote commands fit naturally as another producer that sends `Msg` into the same channel.
- `src/app.rs` is the single state owner. Queue mutation, history saves, EQ re-application,
  prefetching, lyrics refresh, and repeat/shuffle behavior all flow through `App::update()` and
  helper methods such as `on_player_action()`, `advance()`, and `load_song()`.
- `src/player/*` already controls mpv over IPC. A second external CLI should not talk to mpv
  directly because that bypasses app state and creates drift between what mpv is doing and what the
  TUI thinks is happening.
- `src/app.rs` is already a large god module at 3,146 lines. New IPC transport and command parsing
  should live in new modules rather than being added inline.
- `interprocess` is already in `Cargo.toml` with the `tokio` feature, so a remote-control socket can
  be added without a new IPC dependency.
- `directories::ProjectDirs` already determines config/cache/data paths; it also supports
  `runtime_dir()` on Linux. macOS and Windows return no runtime dir, so endpoint selection needs a
  fallback.

## Recommended Direction

Build a small app-level IPC layer owned by the running TUI process.

The TUI remains the only process that owns mpv, the queue, the library, and user-visible state.
Short-lived invocations such as `ytt n` become remote clients: they connect to the running TUI,
send one semantic command, print a short result, and exit.

Do not expose mpv IPC as the public interface. mpv commands such as `playlist-next`, `cycle pause`,
or `set_property volume` are tempting, but this app's correct behavior is more than mpv playback:
manual next ignores repeat-one, loads prefetched URLs, records history, saves library state, refreshes
lyrics, and triggers autoplay/radio top-up. Those are app-level rules, not mpv-level rules.

## User-Facing CLI Shape

Recommended v1 commands:

```text
ytt                  # start the TUI, unless another instance is already running
ytt n | next         # next track
ytt p | prev         # previous track
ytt pp | toggle      # play/pause
ytt up | vol-up      # volume +5
ytt down | vol-down  # volume -5
ytt back             # seek -10s
ytt fwd              # seek +10s
ytt radio            # toggle autoplay radio
ytt quit             # ask the running TUI to quit
```

Recommended v2 commands:

```text
ytt status           # print current track, pause state, volume, queue position
ytt volume 35        # set absolute volume
ytt seek +30         # relative seek
ytt seek 1:20        # absolute seek
ytt alias n next     # optional later, only if custom command aliases prove useful
```

The short aliases should be fixed initially. Reusing the in-app keymap as remote command syntax
sounds elegant, but it is semantically leaky: a physical key means different things in Search,
Settings, Library, and text-entry contexts. Remote commands should be semantic and mode-independent.

## Process Model

Startup should split into two paths before terminal initialization:

1. Parse `std::env::args_os()`.
2. If there is a remote command, run client mode:
   - Resolve the app endpoint and session token.
   - Connect to the running instance.
   - Send one newline-delimited JSON request.
   - Read one JSON response or time out quickly.
   - Exit without loading config-heavy UI state or touching crossterm raw mode.
3. If there is no remote command, run TUI mode:
   - Check for an existing live instance.
   - If one exists, print `ytt is already running` and exit nonzero.
   - If no live instance exists, bind the remote listener.
   - Continue with current config, terminal, actors, and UI loop.

This satisfies the "no duplicate player" goal while still allowing duplicate process invocations in
remote-client mode.

## IPC Protocol

Use newline-delimited JSON over `interprocess::local_socket`.

Example request:

```json
{"version":1,"token":"<per-run-token>","command":"next"}
```

Example response:

```json
{"ok":true,"message":"accepted"}
```

For v1, "accepted" is enough for transport commands. For `status`, add a response path that lets
the run loop snapshot app state and reply after applying any pending state changes.

Protocol types should live in a new `src/remote/proto.rs` module:

```rust
pub enum RemoteCommand {
    Next,
    Prev,
    TogglePause,
    VolumeUp,
    VolumeDown,
    SeekRelative(f64),
    ToggleRadio,
    Quit,
    Status,
}

pub struct RemoteRequest {
    pub version: u8,
    pub token: String,
    pub command: RemoteCommand,
}

pub struct RemoteResponse {
    pub ok: bool,
    pub message: String,
}
```

## Endpoint and Single-Instance Design

Use a stable per-user endpoint, not the existing per-process mpv socket.

Recommended endpoint strategy:

- Linux: prefer `ProjectDirs::runtime_dir()/remote.sock` when available.
- macOS/Linux fallback: use a short per-user temporary directory such as
  `$TMPDIR/ytm-tui/remote.sock`. Avoid deep config/cache paths because Unix socket path length is
  limited on many systems.
- Windows: use a named pipe such as `\\.\pipe\ytm-tui-<user-or-session-hash>`.

Single-instance check should not rely only on a socket file existing:

1. Try to connect to the endpoint with a short timeout.
2. If a server responds, treat the instance as live.
3. If connect fails, consider any Unix socket path stale and remove it before binding.
4. Write an `instance.json` with app pid, endpoint, created-at timestamp, and a random token.
5. On clean shutdown, remove the Unix socket and `instance.json`.
6. On startup, use `sysinfo` as a backstop to verify a recorded pid before deleting stale files.

Do not enable "overwrite active listener" behavior. A new TUI process must never silently displace
the active remote endpoint.

## Security and Abuse Surface

The feature is local-only but still needs a minimal guard because the endpoint name is guessable.

Recommended v1 guard:

- Generate a random per-run token when the TUI starts.
- Store it in `instance.json` in a user-owned app runtime/cache directory.
- Require every remote request to include that token.
- On Unix, create the runtime directory with owner-only permissions where possible.
- Keep v1 commands non-destructive: transport, volume, seek, radio, quit.

This does not defend against the same OS user intentionally controlling their own player, but it
does prevent accidental or cross-user control when file permissions are respected.

## Reducer Integration

Add one app-level entry point instead of faking `KeyEvent`s:

```rust
impl App {
    pub fn apply_remote(&mut self, cmd: RemoteCommand) -> Vec<Cmd> {
        match cmd {
            RemoteCommand::Next => self.on_player_action(Action::NextTrack),
            RemoteCommand::Prev => self.on_player_action(Action::PrevTrack),
            RemoteCommand::TogglePause => self.on_player_action(Action::TogglePause),
            RemoteCommand::VolumeUp => self.on_player_action(Action::VolUp),
            RemoteCommand::VolumeDown => self.on_player_action(Action::VolDown),
            RemoteCommand::SeekRelative(secs) => vec![Cmd::Player(PlayerCmd::SeekRelative(secs))],
            RemoteCommand::ToggleRadio => { /* direct state toggle */ }
            RemoteCommand::Quit => { self.should_quit = true; Vec::new() }
            RemoteCommand::Status => Vec::new(),
        }
    }
}
```

Using semantic commands avoids mode-dependent behavior. For example, `ytt n` should skip track even
if the visible TUI is currently on the Search input screen where typing `n` would normally edit the
query.

The exact public/private boundary will need a small refactor because `on_player_action()` is
currently private. Keep that refactor narrow: either expose a small `apply_player_action()` wrapper
or place `apply_remote()` in `app.rs` while keeping transport code in `remote/`.

## Module Plan

Add:

- `src/remote/mod.rs`: public facade, endpoint resolution, server/client entry points.
- `src/remote/proto.rs`: request/response structs, command enum, JSON parse/format tests.
- `src/remote/args.rs`: small manual parser for short commands.
- `src/remote/server.rs`: Tokio listener loop; accepts one request per connection, validates token,
  forwards `Msg::Remote(command)`.
- `src/remote/client.rs`: connects, sends request, waits for response, prints CLI result.

Modify:

- `src/main.rs`: parse args before terminal init; add client mode; bind remote listener in TUI mode;
  add `Msg::Remote` dispatch in the main loop.
- `src/app.rs`: add `Msg::Remote(RemoteCommand)` and `apply_remote()`.
- `src/config.rs` only if a user-facing setting is needed later. Avoid config changes in v1.
- `README.md`: document remote commands and single-instance behavior.

Avoid:

- Adding IPC code to `src/player/*`; that module should stay focused on app-to-mpv control.
- Adding all command parsing to `main.rs`; keep startup orchestration thin.
- Adding a full CLI framework unless command surface grows. Manual parsing is enough for v1 and
  avoids binary-size/build-time churn.

## Implementation Milestones

### M1: Protocol and CLI parsing

- Add `RemoteCommand`, `RemoteRequest`, `RemoteResponse`.
- Add parser for the fixed v1 aliases.
- Add unit tests for accepted aliases, unknown commands, and `ytt` with no args.
- No socket work yet.

### M2: Endpoint and client

- Add endpoint resolution with platform-specific path/name handling.
- Add token/instance metadata read.
- Implement `ytt n` client path that fails clearly when no instance is running.
- Verify the client path exits without terminal raw mode.

### M3: Server loop inside TUI

- Bind listener after duplicate-instance check and before entering the UI event loop.
- Spawn an accept loop that forwards `RemoteCommand`s into `worker_tx`.
- Keep responses simple: `accepted`, `bad token`, `bad command`, `not running`.
- Ensure shutdown drops the listener and cleans stale Unix socket files.

### M4: Reducer command handling

- Add `Msg::Remote(RemoteCommand)`.
- Route remote transport commands through app-level semantics, not mpv direct commands.
- Add unit tests that remote commands work while `Mode::Search` and `Mode::Settings` are active.
- Add tests that `RemoteCommand::Next` follows the same queue behavior as `Action::NextTrack`.

### M5: Single-instance policy

- Before starting TUI mode, connect-probe the endpoint.
- If a live instance responds, exit with a clear message.
- If only stale metadata/socket exists, clean it and continue.
- Add tests around endpoint stale-file cleanup where feasible.

### M6: Documentation and packaging

- Update README examples.
- Consider whether package-manager install should also install a `ytm` alias. Default should remain
  `ytt` unless renaming is intentional.
- Add release notes warning that `ytt` no longer starts a second independent player.

### M7: Status and future integrations

- Add `ytt status` with a one-shot response channel from run loop to server.
- Add optional MPRIS bridge on Linux if desktop media keys become a goal.
- Investigate Windows SMTC and macOS MPRemoteCommandCenter only after app IPC is stable.

## Pros

- Fast terminal workflow: `ytt n` works from any terminal without switching focus.
- Prevents accidental double players and double mpv processes.
- Reuses existing reducer behavior, so history, queue, prefetch, repeat, and EQ remain coherent.
- Adds a foundation for shell scripts, widgets, tray/menu-bar helpers, MPRIS, and other remote UIs.
- Keeps the first implementation small because `interprocess` and Tokio are already present.
- Does not require a full daemon split to deliver the main value.

## Cons

- Adds an IPC protocol that must be versioned and tested.
- Adds stale-socket, duplicate-instance, and shutdown edge cases.
- Introduces local-control security concerns, even if the commands are low risk.
- Cross-platform endpoint behavior differs between Unix sockets and Windows named pipes.
- The running TUI becomes the owner process. If the TUI exits, remote commands stop working.
- Query-style commands such as `status` need a response path and state snapshot design.
- `src/app.rs` needs a careful small refactor or it will become even more overloaded.

## Alternatives Considered

### Direct mpv IPC client

Lowest implementation cost, but not recommended. It cannot update app queue cursor, history,
library persistence, prefetch state, lyrics state, or app status. It would make the UI lie.

### File-based command queue

Simple and portable, but not recommended. It requires polling or filesystem notifications, has
worse latency, needs stale command cleanup, and still needs all the single-instance logic.

### Full MPD-like daemon split

Architecturally clean long term, but much bigger than this feature needs. It would require moving
mpv ownership, API actors, queue/library state, and config reload behavior into a headless server,
then making the TUI a client. This is a good v3 direction only if multiple independent frontends
become a core product goal.

### OS media control integration first

Not recommended before app IPC. MPRIS, Windows SMTC, and macOS MPRemoteCommandCenter are all
platform-specific. App IPC gives those integrations one stable internal control surface later.

## Open Decisions

- Should a second plain `ytt` invocation exit with an error, print status, or forward to the
  running TUI in some future attach mode?
- Should `ytt quit` be included in v1? It is useful, but it is the only command that closes the
  running UI.
- Should the project provide a literal `ytm` alias, or keep `ytt` as the only installed command?
- Should remote aliases ever be user-configurable, or should they remain stable CLI API?
- Should no-running-instance remote commands fail only, or should a flag like `ytt n --start` launch
  the TUI in the background? The latter starts drifting toward daemon behavior.

## Recommended Ship Slice

Ship v1 with:

- `ytt n`, `ytt p`, `ytt pp`, volume up/down, seek back/forward, radio toggle.
- Single-instance guard for the TUI.
- No `status` yet.
- No configurable remote aliases yet.
- No OS media-control integrations yet.

This gives the main user value with the lowest architecture risk. The design also leaves a clean
path to a later daemon split if remote control becomes central rather than a convenience feature.
