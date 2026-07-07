# ytm-tui TUI Music Hardening Follow-up Plan

Date: 2026-07-07

Scope: follow-up plan after reviewing the supplied security/stability assessments against the
local checkout. This plan targets improvements that are reasonable for a personal/local TUI music
client with a background daemon and local remote-control companion. It does not try to turn
`ytm-tui` into an Internet-exposed multi-tenant service.

This is an ignored local planning document. For this hardening run, it is tracked by explicit user
request so each completed implementation item can be checked and committed.

## Summary

The supplied reviews are directionally accurate, but several claims need tighter scoping against
the current code:

- API work is bounded by a `mpsc::channel(512)` and user-facing stale search results are already
  dropped by request id, but FIFO execution can still waste time on stale searches.
- A central playable URL string validator already exists, but it does not validate DNS resolution
  results or redirects.
- Artwork fetch and resize paths intentionally drain to latest, but both still use unbounded
  channels before the drain point.
- Persist writes coalesce per store, but the inbox is unbounded and failed writes are logged rather
  than retained for retry.
- Remote command validation is much stronger than a naive local socket protocol, but the intended
  security model is still same-OS-user scoped.

Recommended implementation order:

1. P0: URL destination guard design and initial enforcement for arbitrary playable URLs.
2. P0: Replace unbounded artwork/art-resize queues with true latest-only bounded state.
3. P1: Cap and sanitize local TUI search/filter input and redact query logs.
4. P1: Add persist retry/backoff and convert persist inbox to bounded/latest-wins notification.
5. P1: Tighten cookies-file handoff to mpv/yt-dlp.
6. P2: Playlist load repair and secret-bearing backup rotation.
7. P2/P3: API search cancellation/coalescing, remote threat-model documentation, unsafe override
   warnings, and mpv Unix descendant cleanup.

## Execution Checklist

- [x] P0-1: DNS and redirect-aware playable URL destination guard.
- [x] P0-2: Replace artwork and resize unbounded queues.
- [x] P1-1: Cap TUI search, filter, and paste input; redact query logs.
- [x] P1-2: Add persist retry/backoff and bounded/latest-wins notification.
- [x] P1-3: Harden cookies-file handoff to external tools.
- [x] P2-1: Repair playlists on load.
- [x] P2-2: Rotate secret-bearing backups.
- [ ] P2-3: Coalesce/cancel stale API search work.
- [ ] P2-4: Document and self-check the remote same-user security model.
- [ ] P3-1: Warn on unsafe tool/mpv overrides.
- [ ] P3-2: Measure Unix mpv descendant cleanup before changing playback lifetime.

## Ground Rules

- Do not launch `ytt`, `ytt-desktop`, or `cargo run`; the app can play real audio and write real
  user config. Runtime verification must use the project-owned isolated verify workflow.
- Before editing playback, overlay, animation, packaging, or release-sensitive paths, re-read
  `.claude/harness/risk-map.md`.
- Do not edit `.github/workflows/**`, `packaging/**`, `.claude/**`, `AGENTS.md`, `CLAUDE.md`, or
  lockfiles unless a later implementation prompt explicitly authorizes it.
- Warnings count as failures. The native gate remains:
  `cargo fmt --all --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace`.
- Canonical gate for final hardening batches:
  `~/.fable-harness/bin/run-gates .`.
- Because this is a TUI app, visible status messages are part of the fix. Every rejected URL,
  capped paste, failed persist retry, or cookies-file rejection needs a short actionable status
  string.

## External References

- OWASP SSRF Prevention Cheat Sheet: recommends allowlists where possible, warns that redirects
  can bypass input validation, and recommends checking DNS A/AAAA results for non-public IPs.
  <https://cheatsheetseries.owasp.org/cheatsheets/Server_Side_Request_Forgery_Prevention_Cheat_Sheet.html>
- reqwest redirect policy: default clients follow up to 10 redirects; `Policy::none`, `limited`,
  and `custom` are available.
  <https://docs.rs/reqwest/latest/reqwest/redirect/index.html>
  <https://docs.rs/reqwest/latest/reqwest/redirect/struct.Policy.html>
- Tokio unbounded mpsc: unbounded channels provide no backpressure and can buffer until process
  memory is exhausted.
  <https://docs.rs/tokio/latest/tokio/sync/mpsc/fn.unbounded_channel.html>
- Tokio watch channel: retains only the latest sent value, matching latest-only artwork/resize
  semantics.
  <https://docs.rs/tokio/latest/tokio/sync/watch/index.html>
- Tokio DNS helper: `tokio::net::lookup_host` performs DNS resolution and returns socket
  addresses, with the caveat that advanced DNS needs may require a specialized resolver.
  <https://docs.rs/tokio/latest/tokio/net/fn.lookup_host.html>
- Tokio cancellation primitives: `CancellationToken` can signal cancellation; `JoinHandle` drop
  detaches rather than cancels; `AbortHandle::abort` does not stop already-running
  `spawn_blocking` work.
  <https://docs.rs/tokio-util/latest/tokio_util/sync/struct.CancellationToken.html>
  <https://docs.rs/tokio/latest/tokio/task/struct.JoinHandle.html>
  <https://docs.rs/tokio/latest/tokio/task/struct.AbortHandle.html>
- crossterm events: bracketed paste can produce `Event::Paste` when enabled, useful for whole-paste
  validation instead of character-by-character growth.
  <https://docs.rs/crossterm/latest/crossterm/event/index.html>
- IANA special-purpose address registries: use as the policy source for non-public IPv4/IPv6
  ranges beyond Rust's built-in helpers.
  <https://www.iana.org/assignments/iana-ipv4-special-registry>
  <https://www.iana.org/assignments/iana-ipv6-special-registry/iana-ipv6-special-registry.xhtml>

## Finding Triage

| Review item | Plan decision | Priority | Rationale |
|---|---:|---:|---|
| DNS/redirect-aware playable URL validation | Accept with design spike | P0 | Highest value for external catalogs and arbitrary radio streams. |
| API actor lane split/cancellation | Accept narrowed | P2 | Current queue is bounded and stale UI results are dropped; improve responsiveness, not crash safety. |
| Artwork/art-resize unbounded channels | Accept | P0 | Current latest-only drain happens after an unbounded buffer. Tokio docs call this a real OOM risk. |
| Cookies-file symlink handoff | Accept | P1 | Credential boundary; current `is_file()` intentionally follows symlinks. |
| Persist write retry | Accept | P1 | Silent save failure is a trust problem for likes, queue, settings, playlists. |
| Persist unbounded channel | Accept | P1 | Coalescing map exists, but the inbox can still grow before coalescing. |
| Playlist load repair | Accept | P2 | Low risk to implement, protects hand-edited or old files. |
| Search query cap and log redaction | Accept | P1 | Easy, consistent with remote protocol cap, improves paste and privacy handling. |
| `YTM_MPV_EXTRA` unsafe warning | Accept as warning/docs | P3 | User-owned escape hatch is valid for a personal app; warn instead of blocking. |
| Unix mpv descendant cleanup | Measure first | P3 | yt-dlp helper cleanup already improved; mpv process-group changes touch playback lifetime. |
| Remote same-user model | Document and self-check | P2 | Strong for local-user companion, not meant to defeat same-user malware. |
| Music quality heuristics | Defer to product tuning | P3 | False positives/negatives are product quality, not hardening-critical. |

## P0-1: DNS And Redirect-Aware Playable URL Destination Guard

Status: completed in this hardening run.

Implemented:

- Added `src/api/url_guard.rs` with DNS resolution checks, extra special-purpose IPv4/IPv6
  blocking, IPv4-mapped IPv6 handling, and manual redirect probing with automatic redirects
  disabled.
- Added `validate_playable_url_destination` for arbitrary provider/direct/radio URLs and
  `validate_playback_target_for_handoff` for mpv handoff.
- Kept app-generated YouTube/googlevideo URLs on the existing typed/string policy to avoid adding
  network probes to normal YouTube playback.
- Enforced the guard before mpv `loadfile`, before download yt-dlp execution, and before resolver
  yt-dlp execution.
- Added unit coverage for special ranges, mixed DNS answers, redirect-target validation, trusted
  YouTube handoff, and new error display strings.

Original evidence:

- `src/api/mod.rs:210-247` validates raw playable URL strings.
- `src/api/mod.rs:249-267` blocks IP literals that are unspecified, loopback, private,
  link-local, multicast, broadcast, or IPv6 unique-local.
- Provider parsers call this validator before constructing Jamendo, Internet Archive,
  Radio Browser, and non-YouTube yt-dlp playable refs.
- `Song::playback_target_checked()` and download handoff call the same validator again.
- The current validator does not resolve DNS names or inspect redirect targets.

Why this is reasonable for a TUI music app:

- Radio Browser and some external providers intentionally return arbitrary stream URLs.
- The app is local, but it can hand remote catalog URLs to mpv/yt-dlp, which can access local
  network resources from the user's machine.
- Blocking private/LAN/metadata destinations by default is a good privacy and surprise-reduction
  policy. Users who intentionally play a LAN radio server can opt in explicitly.

Design:

1. Keep the existing synchronous `validate_playable_url` as the cheap string policy.
2. Add an async network destination guard, probably in `src/api/url_guard.rs`.
3. Define two levels:
   - `validate_playable_url_string(source, raw) -> Url`.
   - `validate_playable_url_destination(source, url, policy) -> ValidatedPlayableUrl`.
4. Policy defaults:
   - `http` and `https` only.
   - no credentials.
   - no localhost or `.localhost`.
   - DNS resolution must produce at least one address.
   - every resolved A/AAAA address must be public according to project policy.
   - block IPv4-mapped IPv6 addresses if the embedded IPv4 would be blocked.
   - block IANA special-purpose ranges that Rust helpers do not cover, including shared address
     space, benchmarking, documentation, protocol-assignment ranges, and metadata/link-local ranges.
5. Add a config flag only if needed after user decision:
   - default: `allow_private_playable_urls = false`;
   - opt-in: allow private/LAN stream targets with a clear "unsafe local streams enabled" status
     and doctor line.
6. Do not add dependencies just for CIDR matching unless explicitly approved. Manual prefix checks
   are enough for the small policy table.

Redirect handling:

1. reqwest-controlled provider API requests should use a central provider client policy.
   - Fixed provider API hosts can stay on normal reqwest behavior if their URLs are constants.
   - Any request to a provider-returned playable/media URL should disable automatic redirects and
     follow redirects manually with per-hop validation.
2. For direct playable streams handed to mpv:
   - preferred: resolve and validate initial URL before handoff;
   - for arbitrary public stream providers, do a small preflight that manually follows up to 5
     redirects and validates each target;
   - if preflight succeeds, pass the final URL to mpv;
   - if preflight cannot determine redirect behavior because a stream refuses HEAD, fall back to a
     bounded GET probe with `Range: bytes=0-0` where safe, or fail with an actionable status.
3. For yt-dlp URLs:
   - YouTube IDs should keep using typed YouTube refs.
   - Non-YouTube `YtdlpUrl` from external search should pass the same initial DNS check.
   - Full redirect control inside yt-dlp is not guaranteed, so source-specific allowlists matter.

Source-specific policy:

- YouTube generated watch URLs: typed source; validate host is a known YouTube domain at string
  construction, no external arbitrary URL.
- Audius generated stream URL: typed generated URL; validate expected Audius API host.
- Jamendo: allow expected Jamendo media/API hosts where possible; otherwise apply public-DNS guard.
- Internet Archive: allow expected archive.org/download hosts where possible; otherwise apply
  public-DNS guard.
- Radio Browser: arbitrary public stream mode; always apply public-DNS and redirect guard.
- SoundCloud/non-YouTube yt-dlp search: apply public-DNS guard, and keep a source label in errors.

Tests:

- Unit tests for string policy continue to cover unsupported schemes, credentials, localhost, and
  IP literals.
- Add resolver-injection tests for hostname-to-loopback, hostname-to-private, hostname-to-link-local,
  hostname-to-mixed-public-private, and hostname-to-public.
- Add IPv4-mapped IPv6 tests.
- Add redirect policy tests with a local test HTTP server:
  - public URL to private redirect is rejected;
  - redirect loops stop at the configured limit;
  - cross-scheme redirects are validated;
  - credentials in redirect target are rejected.
- Provider parser tests assert rejected rows are skipped and logged without panic.
- Playback/download boundary tests reject a manually constructed `Song` whose URL passes string
  validation but fails destination validation.

Acceptance criteria:

- No arbitrary provider URL reaches mpv/yt-dlp unless the current policy has validated its network
  destination or the user explicitly opted into private playable URLs.
- Rejections produce a short status or provider warning, not a silent empty result where feasible.
- Normal YouTube playback remains unaffected.

## P0-2: Replace Artwork And Resize Unbounded Queues

Status: completed in this hardening run.

Implemented:

- `src/artwork.rs` now uses a `tokio::sync::watch` latest-value channel for TUI album-art fetch
  requests.
- `crates/ratatui-image` now uses bounded tokio `mpsc::Sender` for threaded resize requests and
  restores the protocol for retry when the bounded queue is full or closed.
- `src/main.rs` now uses `backpressure::ART_RESIZE_QUEUE` for art resize work.
- `src/media/artwork.rs` now uses a bounded `MEDIA_ARTWORK_QUEUE` for media-session artwork cache
  requests.

Original evidence:

- `src/artwork.rs:16` imports `UnboundedReceiver` and `UnboundedSender`.
- `src/artwork.rs:49-64` stores an `UnboundedSender<ArtworkCmd>`.
- `src/artwork.rs:68-104` drains queued artwork commands to the latest only after receiving.
- `src/main.rs:746-760` creates an unbounded resize queue and drains it to the latest only after
  the worker wakes.
- `src/util/backpressure.rs:52-55` already declares `ART_RESIZE_QUEUE` as capacity 8, but the
  current resize path does not use it.

Why this is reasonable for a TUI music app:

- Track skips, album-art fetches, and terminal resize events are high-frequency user actions.
- The UI only needs the latest artwork for the current track and latest resize for the current
  layout.
- Unbounded buffering contradicts the documented latest-only intent.

Implementation plan:

1. Replace artwork fetch channel with one of:
   - `tokio::sync::watch` carrying `Option<ArtworkCmd>` plus generation id; or
   - bounded `mpsc` capacity 1/8 with explicit drop-old semantics.
2. Prefer `watch` for artwork fetch because only the newest track art matters.
3. Replace art resize channel with the existing `backpressure::ART_RESIZE_QUEUE` or a `watch`
   channel keyed by a monotonically increasing resize id.
4. Keep current result-side stale guards:
   - artwork result must still match current video id;
   - resize result must still match current art/layout generation.
5. Add generation ids to both request and response if not already sufficient.
6. Do not attempt to abort already-running `spawn_blocking` resize/decode work; Tokio documents
   that already-running blocking tasks cannot be aborted. Instead, cap queued work and discard stale
   results.
7. Add a small resize debounce, initially 50-100 ms, only if tests show resize storms still spend
   too much CPU.

Tests:

- Sending many artwork requests while the actor is busy leaves only the latest pending request.
- Sending many resize requests while the blocking worker is busy does not increase buffered count
  beyond the configured capacity.
- A stale artwork result is dropped when current track id changed.
- A stale resize response is dropped when generation changed.
- Existing artwork decode caps and missing-art tests continue to pass.

Acceptance criteria:

- No artwork or resize path uses `mpsc::unbounded_channel` without a written justification.
- Flooding skip/resize events cannot grow memory through these channels.

## P1-1: Cap TUI Search, Filter, And Paste Input

Status: completed in this hardening run.

Implemented:

- Added `src/util/query.rs` with shared search/filter byte caps, forbidden query-character
  detection, submit sanitization, and safe log preview metadata.
- Reused the shared 2048-byte search cap from the remote protocol.
- Capped local TUI search input at 2048 bytes and search/library filters at 512 bytes.
- Rejected control, bidi-control, and zero-width-control characters in TUI query entry and submit.
- Replaced API/daemon full-query tracing fields with `{bytes, chars, preview, truncated}`.
- Added reducer and util tests for over-cap input, forbidden characters, submit revalidation, and
  log preview behavior.

Original evidence:

- `src/remote/proto/command.rs:16` caps remote query bytes at 2048.
- `src/remote/proto/command.rs:150-160` rejects empty, too-long, and control-character remote
  queries.
- `src/app/search.rs:68-82` appends typed chars to `self.search.input` without a cap.
- `src/app/search.rs:399-405` appends typed chars to search-filter popup query without a cap.
- `src/app/library.rs:92-101` appends typed chars to library filter query without a cap.
- `src/app/search.rs:412-440` submits the full trimmed TUI query downstream.
- `src/api/mod.rs:988`, `src/api/mod.rs:1018`, `src/api/mod.rs:1039`, and `src/api/mod.rs:1048`
  log full search/resolve query text.

Why this is reasonable for a TUI music app:

- Users paste URLs and text into terminal inputs.
- Very large paste input can affect rendering, logs, API queue memory, and yt-dlp query strings.
- Remote and TUI should share the same semantic cap.

Implementation plan:

1. Add a shared query policy module, for example `src/search_input.rs` or `src/util/query.rs`.
2. Use constants:
   - `MAX_SEARCH_QUERY_BYTES = remote::proto::REMOTE_MAX_QUERY_BYTES` or a duplicated public
     constant if avoiding a dependency from app code into remote protocol.
   - `MAX_FILTER_QUERY_BYTES = 512` for library/search result filters.
   - `MAX_QUERY_PREVIEW_CHARS = 32` for logs/status previews.
3. Implement helpers:
   - `try_push_query_char(buf, ch, max_bytes) -> QueryEditResult`;
   - `sanitize_query_for_submit(raw) -> Result<String, QueryRejectReason>`;
   - `query_log_preview(query) -> { bytes, chars, preview }`.
4. Strip or reject:
   - NUL and control characters;
   - bidi override/control characters;
   - extreme zero-width controls.
5. Preserve normal non-English search text.
6. On cap hit:
   - do not append the extra char;
   - set a status like `Search query is too long`;
   - keep focus in the input.
7. If crossterm bracketed paste is enabled later, handle `Event::Paste(String)` as one operation
   so the app can truncate/reject once rather than show repeated status churn.
8. Replace full query tracing with length/source/request id plus a safe preview. Keep full query
   out of logs by default.

Tests:

- TUI search input caps at the byte limit without splitting UTF-8.
- Search submit rejects a too-long preloaded string.
- Library filter and search-filter popup cap at their filter limit.
- Control and bidi characters do not enter query buffers.
- Korean/Japanese/emoji input remains valid until the byte cap.
- API log helper never returns the full long query.
- Existing URL-paste YouTube parsing still works for normal URLs.

Acceptance criteria:

- TUI input has no path to send a query larger than the shared cap.
- Logs do not include full user search queries by default.

## P1-2: Persist Retry, Backoff, And Bounded Latest-Wins Inbox

Current evidence:

- `src/persist.rs:119-151` uses `mpsc::UnboundedSender<PersistMsg>` and
  `mpsc::unbounded_channel()`.
- `src/persist.rs:178-230` coalesces pending snapshots per store in `SharedPending`.
- `src/persist.rs:247-269` removes a snapshot from pending before writing.
- `src/persist.rs:259-261` logs write failure but does not reinsert the snapshot.

Why this is reasonable for a TUI music app:

- Likes, ratings, local playlists, download manifest, and settings should not appear saved when a
  transient disk error occurred.
- Coalescing already exists, so bounded notification can be added without losing latest snapshots.

Implementation plan:

1. Convert the inbox from payload-carrying unbounded messages to bounded latest-wins notification.
2. New shape:
   - `PersistHandle::save(snapshot)` inserts/replaces the snapshot in `SharedPending`.
   - It then `try_send(PersistMsg::Dirty(kind))` on a small bounded channel or calls `Notify`.
   - If the channel is full, do not panic and do not allocate; the snapshot is already in pending.
3. Keep flush and delete commands reliable:
   - `Flush` should have a control path that can make progress even if dirty notifications are
     saturated.
   - `DeleteRomanizedTitles` must preserve ordering so an older cache save cannot resurrect the
     file.
4. Add retry state per `StoreKind`:
   - `retry_count`;
   - `retry_not_before`;
   - `last_error`;
   - `last_failed_at`.
5. On `Ok((Err(e), snapshot))`, reinsert the snapshot unless a newer snapshot for the same store is
   already pending.
6. Use exponential backoff with cap:
   - start 500 ms or 1 s;
   - double up to 30 s;
   - reset on success.
7. On `spawn_blocking` join failure, the moved snapshot may be unavailable. Treat this as a hard
   actor error, log it, and rely on the next mutation or flush; optionally make `Snapshot` cloneable
   if preserving this rare path is worth the cost.
8. Expose persist health:
   - internal event `PersistEvent::WriteFailed { store, error }`;
   - status line for high-value stores after the first failure;
   - doctor/status display with `last_persist_error`.
9. Flush semantics:
   - `flush(budget)` should attempt pending writes and any due retries within the budget.
   - Return `false` if writes are still dirty or failed.
   - Quit path should surface failure where possible.

Tests:

- Save flood cannot grow the channel; latest snapshot remains in pending.
- Failed write is retried and eventually succeeds.
- Newer snapshot replaces older failed snapshot before retry.
- Flush returns false when a write repeatedly fails.
- Clean quit reports persist failure status or log.
- `DeleteRomanizedTitles` cannot be followed by an older pending save that recreates the cache.

Acceptance criteria:

- A transient write failure does not silently drop the dirty snapshot.
- Persist notification is bounded or `Notify`-based, not unbounded payload buffering.

Status: completed in this hardening run.

Implementation summary:

- `src/persist.rs` now keeps dirty snapshots only in `SharedPending`; `save(snapshot)` replaces the
  per-store pending entry and wakes the actor through `tokio::sync::Notify`, so snapshot payloads no
  longer accumulate in an unbounded channel.
- `src/util/backpressure.rs` defines a bounded `PERSIST_CONTROL_QUEUE` for flush/delete control
  messages. Dirty notifications cannot saturate this control path.
- Failed writes reinsert the snapshot unless a newer pending snapshot already exists for that
  store. Retry state tracks retry count, retry deadline, last error, and last failed-at timestamp,
  with 500 ms exponential backoff capped at 30 seconds and reset on success.
- `PersistHandle::flush(budget)` shares one deadline across command send and ack wait, attempts all
  pending writes, and returns `false` if any store remains dirty after a failed write.
- First failures for user-visible stores emit `PersistEvent::WriteFailed { store, error }`; the
  standalone runtime maps that to `Msg::PersistFailed` and shows a status-line save failure. Clean
  quit fallback logs direct-write failures per store. The stable remote status wire schema was not
  expanded in this item.
- `DeleteRomanizedTitles` removes older pending cache snapshots before queuing the actor-side
  delete, preserving the no-resurrection ordering intent.

Verification:

- `cargo test persist --lib`
- `cargo fmt --all --check`
- `cargo clippy --workspace --all-targets -- -D warnings`
- `~/.fable-harness/bin/run-gates .`

## P1-3: Cookies-File Handoff Hardening

Status: completed in this hardening run.

Implementation summary:

- `Config::existing_cookies_file()` now rejects symlinks, Windows reparse points, non-regular
  files, and files larger than the existing 4 MiB cookie cap before any external-tool handoff.
- `Config::cookies_file_for_external_tools(_with_warning)` imports a valid cookies file into the
  private app data directory as `cookies.external.txt` via `safe_fs::write_private_atomic`, so
  mpv/yt-dlp receive an app-owned private copy when a data dir is available.
- Startup playback/download runtime setup, daemon player spawn, and video overlay spawn now use the
  hardened external-tool cookies helper. Inline cookie-header parsing remains unchanged.
- Rejection/import warnings avoid printing the cookies path. The TUI startup and video overlay
  paths surface actionable status text when an explicitly configured cookies file cannot be used.
- Cookie hardening tests now cover missing files, real files, symlink rejection, oversized rejection,
  private imported copies, strict-source fallback when no data dir exists, and preserved mpv/yt-dlp
  cookie argument handoff.

Verification:

- `cargo test cookies_file --lib`
- `cargo test external_tool_cookies --lib`
- `cargo test passes_cookie_file --lib`
- `cargo fmt --all --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace`
- `~/.fable-harness/bin/run-gates .`

Current evidence:

- `src/config.rs:825-830` reads cookies for header parsing with `read_no_symlink_limited`.
- `src/config.rs:849-854` returns `existing_cookies_file()` using `path.is_file()`, which follows
  symlinks.
- `src/player/mpv.rs:153-158` forwards the cookies path to mpv's yt-dlp hook.
- `src/download.rs:198-199` forwards the cookies path to yt-dlp.

Why this is reasonable for a TUI music app:

- Cookies are credentials.
- Passing a path to an external process is a different trust boundary from parsing a cookie header
  inside the app.
- The current comment explicitly chooses compatibility with symlinked cookie exports; the follow-up
  should choose either strictness or an import workflow.

Implementation plan:

1. Replace `existing_cookies_file()` with a stricter helper:
   - reject missing paths;
   - reject symlinks/reparse points;
   - require regular file;
   - require file size <= `MAX_COOKIE_BYTES`;
   - optionally warn if Unix permissions are world-readable.
2. To preserve user workflow, add one of:
   - strict mode only: users must point at a real exported file;
   - safer compatibility: copy/import the cookie file into a private app-owned file and pass only
     that private copy to mpv/yt-dlp.
3. Preferred TUI-friendly approach:
   - On startup, if configured cookies file is valid and changed, copy it to
     `<data dir>/cookies.external.txt` with private permissions.
   - Pass the private copy to external tools.
   - If the source is a symlink, reject with a status/doctor warning and do not import.
4. Redact cookies path in logs unless the path is needed for `doctor --verbose`.
5. Keep inline cookie header behavior unchanged.

Tests:

- Existing real cookies file is accepted.
- Missing file returns `None`.
- Symlink cookies file is rejected on Unix and Windows reparse-point path where testable.
- Oversized cookies file is rejected for external handoff.
- Private imported copy is `0600` on Unix and lives under app data dir.
- mpv/download tests assert the private copy path is passed when import mode is used.

Acceptance criteria:

- External tools never receive a symlinked cookies path by default.
- Users get a clear remediation message.

## P1-4: API Search Latest-Wins And Cancellation

Current evidence:

- `src/api/mod.rs:905` uses a bounded API command channel.
- `src/api/mod.rs:970-1240` processes `ApiCmd` FIFO in one actor loop.
- `src/app/search.rs:420-422` stamps searches with request ids.
- `src/app/mod.rs:1188-1239` drops stale search results/errors by request id.
- `src/api/ytmusic.rs:332-380` bounds multi-source search by an operation deadline plus current
  provider timeout.

Why this is reasonable for a TUI music app:

- This is primarily responsiveness, not memory safety.
- A user typing or submitting several searches should see the latest query win quickly.
- Playback commands already go through player/runtime lanes, so do not overstate this as blocking
  pause/skip itself.

Implementation stages:

Stage A: cheap FIFO coalescing.

1. When the API actor receives `ApiCmd::Search`, drain immediately available queued commands.
2. Keep only the latest TUI `Search` command.
3. Keep non-search commands in order.
4. Do not drop `PlaylistTracks`, `StreamingPreflight`, or mutation-like future commands.
5. Emit a stale/cancelled event for superseded TUI searches only if needed to clear UI state.

Stage B: search lane task.

1. Wrap or clone `YtMusicApi` safely, likely via `Arc<YtMusicApi>` if it is not already cheap to
   clone.
2. Run TUI search and GUI search in separate cancellable tasks.
3. Keep an `AbortHandle` or `JoinHandle` for the current TUI search.
4. On newer search:
   - abort the older search task;
   - emit no result for the older request id;
   - start the new task.
5. Use `CancellationToken` if nested provider calls need cooperative cancellation.
6. Remember that dropping `JoinHandle` detaches; call `abort()` explicitly when needed.
7. Do not expect abort to stop already-running blocking work; rely on existing child-process
   timeout/kill-on-drop behavior where yt-dlp is involved.

Stage C: lane separation.

1. If Stage A/B still leaves visible stalls, split actors:
   - search actor;
   - playlist fetch actor;
   - streaming/recommendation actor;
   - resolve-track actor.
2. Keep shared API auth initialization in one place.
3. Preserve TUI/daemon parity if any playback-mode behavior is touched.

Tests:

- Submitting search A then B before A starts runs only B.
- Submitting search B while A is in-flight aborts or supersedes A; A result cannot clear B state.
- Playlist track fetch is not dropped by search coalescing.
- GUI search tickets remain isolated from TUI search request ids.
- Streaming refill still returns deterministic events under search load.

Acceptance criteria:

- Latest TUI search is not stuck behind multiple stale TUI searches.
- No user-visible command silently disappears without a stale/cancelled state transition.

## P2-1: Playlist Load Repair

Status: completed in this hardening run.

Implemented:

- Added load-time playlist repair with a `PlaylistRepairReport`.
- Enforced playlist count, per-playlist song count, trimmed/non-empty names, sane unique ids, and
  metadata re-sanitization for deserialized songs.
- Startup now persists a repaired snapshot and surfaces a one-line status only when truncation
  removed playlists or tracks.
- Added regression coverage for cap truncation, deterministic id repair, blank-name repair,
  deserialized song sanitization, and unchanged normal playlists.

Verification:

- `cargo test playlists --lib`
- `cargo test repair_loaded --lib`
- `cargo fmt --all --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace`

Current evidence:

- `src/playlists.rs:16-18` defines playlist and songs-per-playlist caps.
- `src/playlists.rs:46-59` load path uses size-bounded JSON load.
- `src/playlists.rs:88-124` create/add paths enforce caps.
- Load comments say playlist entries are not count-truncated and rely on byte cap alone.

Why this is reasonable for a TUI music app:

- Hand-edited JSON and old versions are realistic.
- Repairing at load avoids rendering/search/save slowdowns from huge but under-byte-cap files.

Implementation plan:

1. Add `Playlists::repair_loaded(&mut self) -> PlaylistRepairReport`.
2. Apply after `load_json_or_default_limited`.
3. Enforce:
   - max playlists;
   - max songs per playlist;
   - non-empty name after trim;
   - sane id slug, regenerating if blank or duplicate;
   - provider metadata already sanitized by `Song` constructors, but re-sanitize if deserialized
     old data can bypass constructors.
4. Preserve order and oldest entries by default.
5. If repair changed data, queue a persist so the repaired shape is saved.
6. Surface a one-line status only when truncation occurred.

Tests:

- Oversized count under 50 MB is truncated to caps.
- Duplicate ids are repaired deterministically.
- Blank names are dropped or renamed according to chosen policy.
- Repair report includes counts for playlists removed and songs removed.
- Normal playlist JSON round-trips unchanged.

Acceptance criteria:

- Loaded playlist memory shape obeys the same caps as create/add paths.

## P2-2: Secret-Bearing Backup Rotation And Privacy Doctor

Status: completed in this hardening run.

Implemented:

- Added secret recovery-backup retention to `safe_fs`, keeping the newest three app-managed
  secret backups while preserving the newly moved-aside file.
- Applied the retention policy to `config.json` corrupt and too-large backup paths.
- Added `ytt doctor privacy` to report secret-bearing files and recovery backup counts without
  reading secret contents.
- Added explicit `ytt doctor privacy --cleanup` for retention cleanup of app-managed secret
  backup groups. Configured source `cookies.txt` is reported but not pruned because it can live
  outside app-managed directories.
- Added regression coverage for backup retention, explicit cleanup, config-load rotation, and the
  isolated privacy doctor CLI path.

Verification:

- `cargo test secret_backup --lib`
- `cargo test privacy --lib`
- `cargo test load_from_rotates_secret_recovery_backups --lib`
- `cargo test doctor_privacy_reports_secret_files_without_tui_startup --test cli_smoke`
- `cargo fmt --all --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace`

Current evidence:

- `src/util/safe_fs.rs:587-605` creates numbered backups up to 1000 per label.
- `src/config.rs` tests intentionally preserve original secret material in corrupt/too-large
  backups for recovery.
- This is good recovery behavior, but it can retain old cookie/API/scrobble secrets for a long
  time.

Why this is reasonable for a TUI music app:

- Personal apps should preserve data after corruption, but credentials should not accumulate
  indefinitely.
- A privacy doctor command is safer than silently deleting recovery files.

Implementation plan:

1. Classify persisted files:
   - secret-bearing: config, scrobble tokens, Spotify tokens, cookies private copy;
   - non-secret state: library, downloads, playlists, session, romanize cache.
2. Add backup retention policy:
   - default keep 3 corrupt/too-large/unreadable backups per secret-bearing file;
   - keep more for non-secret state if desired.
3. Do not delete backups during the same operation that creates the first recovery copy unless
   policy is explicit.
4. Add `ytt doctor privacy` or extend existing doctor:
   - list secret-bearing files;
   - list backup counts and ages;
   - recommend cleanup command.
5. Optional cleanup command should require explicit user invocation.

Tests:

- Creating backup N+1 for secret-bearing files rotates the oldest beyond retention.
- Non-secret backup behavior remains unchanged or follows its own documented limit.
- Backup rotation never deletes the file just moved aside.
- Doctor output redacts home path and never prints token/cookie contents.

Acceptance criteria:

- Secret-bearing backups do not grow without an explicit policy.
- Recovery remains possible for the most recent corrupt file.

## P2-3: Remote Control Threat Model And Permission Self-Check

Current evidence:

- Remote uses a per-user runtime dir, descriptor, local socket/named pipe, token, version checks,
  size caps, semantic command validation, session caps, and outbound byte caps.
- The model is still same-OS-user scoped; a malicious process running as the same user can usually
  read the descriptor/token and app data.

Why this is reasonable for a TUI music app:

- The current model is appropriate for a local TUI plus desktop/CLI companion.
- Trying to defend against same-user malware would require much deeper OS-specific isolation and is
  not a good default project goal.

Implementation plan:

1. Add a short `docs/remote-security.md` or README section:
   - remote is local-user scoped;
   - not intended to defend against malicious same-user processes;
   - do not expose sockets or descriptor files over shared directories;
   - remote commands can mutate playback/library/download state.
2. Add startup self-check:
   - Unix runtime dir is owned by current user and mode is 0700;
   - descriptor is regular file and mode is 0600;
   - socket path parent is private;
   - Windows check is best-effort if using the abstraction.
3. If a self-check fails, disable remote for that run and show a status/doctor warning.
4. Consider remote capabilities later:
   - read-only status;
   - playback control;
   - library mutation;
   - download control.
   This is useful for future companion apps, not required for immediate hardening.

Tests:

- Unsafe descriptor permissions disable remote or fail startup of remote only.
- Safe permissions pass.
- Documentation states same-user model clearly.

Acceptance criteria:

- Users and future contributors cannot mistake the token for a same-user security boundary.

## P3-1: Unsafe Override Visibility

Current evidence:

- `src/player/mpv.rs:197-203` appends `YTM_MPV_EXTRA` after safe defaults so user overrides can
  win.
- `src/tools/mod.rs` intentionally supports explicit yt-dlp overrides.

Why this is reasonable for a TUI music app:

- Power-user escape hatches are valuable.
- Hidden escape hatches make debugging and security review harder.

Implementation plan:

1. Keep overrides working.
2. Add `doctor` and startup trace/status warnings:
   - `YTM_MPV_EXTRA is set; safe mpv defaults may be overridden`.
   - `yt-dlp override active: <redacted path/source>`.
3. Optionally reject known-dangerous mpv flags only in daemon/remote service mode, not standalone
   local TUI mode, unless the user asks for strict mode.
4. Add a config/doctor "unsafe overrides" section.

Tests:

- Warning appears when `YTM_MPV_EXTRA` is set.
- No warning when unset.
- Path values are sanitized.

Acceptance criteria:

- Advanced users retain control, but unsafe mode is visible.

## P3-2: Unix mpv Descendant Cleanup Measurement

Current evidence:

- mpv has `kill_on_drop`, signal/panic hook cleanup, PID registry, and Windows Job Object handling.
- yt-dlp/process helper timeout cleanup already has stronger process-group behavior.
- Unix mpv descendant edge cases are plausible but not yet demonstrated in local tests.

Why this is lower priority:

- Touches playback lifetime, a risk-map area.
- mpv normally owns and cleans its helper processes.
- Need evidence before changing process-group semantics for the main player.

Implementation plan:

1. Add a static test or small helper around process lifetime code if feasible without launching
   real mpv.
2. Manual/verify workflow only:
   - use isolated config;
   - fake mpv script that spawns a detached child;
   - assert cleanup behavior.
3. If orphan is reproduced:
   - spawn mpv in its own Unix process group;
   - on lifecycle cleanup, signal group then wait/reap parent;
   - keep Windows Job Object behavior unchanged.
4. Re-run playback lifetime tests and parity tests if touched.

Acceptance criteria:

- Either documented as accepted low risk with evidence, or fixed with a process-group cleanup test.

## Implementation Sequence

### Milestone 1: Quick High-Value Local Backpressure And Input Caps

Files likely touched:

- `src/artwork.rs`
- `src/main.rs`
- `src/app/artwork.rs`
- `src/util/backpressure.rs`
- `src/app/search.rs`
- `src/app/library.rs`
- `src/event.rs` if paste handling is added
- `src/api/mod.rs` for query log redaction

Work:

1. Add query/edit helpers and tests.
2. Cap TUI search input, search-filter popup, and library filter.
3. Redact query tracing.
4. Replace art resize unbounded channel.
5. Replace artwork actor unbounded channel.
6. Run native gate.

### Milestone 2: Persistence And Cookies Reliability

Files likely touched:

- `src/persist.rs`
- `src/config.rs`
- `src/player/mpv.rs`
- `src/download.rs`
- `src/util/safe_fs.rs`
- app/runtime status dispatch if persist/cookies failures become visible

Work:

1. Refactor persist pending notification.
2. Add retry/backoff and persist failure event/status.
3. Add strict cookies-file handoff or private-copy import.
4. Add symlink/oversize tests.
5. Run native gate.

### Milestone 3: URL Destination Guard

Files likely touched:

- `src/api/mod.rs`
- new `src/api/url_guard.rs` or similar
- `src/api/ytmusic.rs`
- `src/download.rs`
- daemon/player handoff path if destination guard runs there
- `src/util/http.rs` if redirect preflight becomes shared

Work:

1. Add resolver abstraction and IP policy tests.
2. Add async destination guard.
3. Add manual redirect preflight for arbitrary stream URLs.
4. Apply source-specific policy.
5. Add user-visible rejection messages where relevant.
6. Run native gate and canonical gate.

### Milestone 4: Responsiveness And Repair

Files likely touched:

- `src/api/mod.rs`
- `src/api/ytmusic.rs`
- `src/playlists.rs`
- app reducer for repair status/persist

Work:

1. Add search command coalescing.
2. Add cancellable search lane if coalescing is not enough.
3. Add playlist load repair and persist-on-repair.
4. Run native gate.

### Milestone 5: Documentation And Low-Priority Hardening

Files likely touched:

- README or `docs/remote-security.md`
- doctor/status modules
- `src/player/mpv.rs`
- `src/tools/mod.rs`
- process lifetime modules if mpv cleanup is fixed

Work:

1. Document remote same-user model.
2. Add remote permission self-check.
3. Add unsafe override visibility.
4. Measure mpv Unix descendant behavior.
5. Implement process-group cleanup only if evidence supports it.
6. Run native gate.

## Verification Plan

For every code milestone:

1. `cargo fmt --all --check`
2. `cargo clippy --workspace --all-targets -- -D warnings`
3. `cargo test --workspace`

For final combined hardening:

1. `~/.fable-harness/bin/run-gates .`
2. Review warnings as failures.
3. Do not run the app directly.

Targeted tests to add:

- URL guard:
  - resolver injection for public/private/mixed DNS;
  - redirect-to-private rejection;
  - source-specific provider row skipping.
- Backpressure:
  - artwork and resize flood tests prove bounded pending work.
- Input:
  - UTF-8 cap boundaries;
  - control/bidi rejection;
  - log preview redaction.
- Persist:
  - failed write retry;
  - bounded notification under flood;
  - flush false on repeated failure.
- Cookies:
  - symlink rejection;
  - oversize rejection;
  - private copy handoff.
- Playlist:
  - repair caps after load;
  - duplicate/blank id behavior.
- Remote:
  - permission self-check pass/fail.

## Ship Criteria

This follow-up plan is complete when:

- Arbitrary provider playable URLs are checked against DNS/IP policy before handoff.
- Redirects are either manually validated for arbitrary stream preflight or explicitly documented
  as a remaining limitation for mpv/yt-dlp.
- Artwork fetch and art resize no longer rely on unbounded queues.
- TUI search/filter input shares a clear cap policy with remote commands.
- Full user search queries are not logged by default.
- Persist write failures keep dirty snapshots for retry and produce visible health information.
- Persist notification cannot grow unbounded.
- External tools do not receive symlinked cookies paths by default.
- Loaded playlists obey object-count caps, not only file-size caps.
- Remote security documentation states the same-user model.
- Unsafe tool/mpv overrides are visible in doctor/status.
