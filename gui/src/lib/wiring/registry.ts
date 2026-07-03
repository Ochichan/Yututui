// The Patch Bay — the single source of truth for every feature whose UI is finished but
// whose wire to the core is not yet connected (the "미완성" surfaces).
//
// ── Contract for follow-up wiring agents ─────────────────────────────────────────────
// 1. Every stubbed call site in the frontend goes through `wip.open(<FeatureId>)` and is
//    tagged with a greppable marker comment:  TODO(wire:<milestone>/<feature-id>)
//    Find all of a feature's seams with:      rg "wire:M2/search.run" gui/src
// 2. To wire a feature: read the brief, build the store per docs/gui/05 §5, replace the
//    `wip.open(...)` call sites with real client calls, extend the demo core
//    (gui/src/lib/dev/democore.ts) so the browser demo still works, then DELETE the
//    registry entry below. gui/tests/wiring.test.ts pins registry ↔ marker consistency.
// 3. `capability` is the forward-compatible auto-dissolve: when the connected core
//    advertises it, the gate opens the real path without a frontend release. Until the
//    core defines these strings they are provisional — keep them in sync with
//    docs/gui/02 §10 when B1+ lands.
//
// Full handoff narrative: gui/WIRING.md.

export type FeatureId =
  | 'library.fetch'
  | 'library.playlists'
  | 'downloads.manage'
  | 'queue.reorder'
  | 'settings.apply'
  | 'settings.hotkeys'
  | 'settings.theme-editor'
  | 'settings.animations'
  | 'settings.accounts'
  | 'ai.chat'
  | 'ai.whygem'
  | 'transfer.wizard'
  | 'radio.mode'
  | 'help.keymap'
  | 'lyrics.live'
  | 'artwork.live'
  | 'i18n.catalog';

export interface WiringSpec {
  /** Human title shown in the not-wired-yet modal. */
  title: string;
  /** Milestone that lands this wire (docs/gui/09-milestones.md). */
  milestone: string;
  /** The spec section that defines "done" for this feature. */
  brief: string;
  /** The protocol surface this feature speaks once wired (docs/gui/02 §13). */
  protocol: string;
  /** Where the frontend seam is: files to touch, stores to create. */
  seam: string;
  /** Optional capability string that auto-opens the gate when the core advertises it. */
  capability?: string;
  /** Extra marching orders for the wiring agent. */
  notes?: string;
}

export const WIRING: Record<FeatureId, WiringSpec> = {
  'library.fetch': {
    title: 'Library pages',
    milestone: 'M2',
    brief: 'docs/gui/07-feature-briefs.md §4',
    protocol:
      'req fetch_library_page { scope, filter, offset, limit } + `library` topic invalidation push; library_play / library_remove',
    seam: 'create gui/src/lib/stores/library.svelte.ts; replace the wip() seams in gui/src/views/LibraryView.svelte',
    capability: 'library-v8',
    notes: 'Windowed paging cursor per scope; filter debounce lives client-side.',
  },
  'library.playlists': {
    title: 'Playlists (drill-down + CRUD modals)',
    milestone: 'M2',
    brief: 'docs/gui/07-feature-briefs.md §5',
    protocol:
      'playlist_create / playlist_delete / playlist_add_tracks / playlist_remove_track / playlist_play; `playlists` topic + req fetch_playlist_detail',
    seam: 'create gui/src/lib/stores/playlists.svelte.ts + gui/src/views/library/PlaylistDetail.svelte + the three modals (Create / Delete / Add-to-playlist)',
    capability: 'library-v8',
  },
  'downloads.manage': {
    title: 'Downloads',
    milestone: 'M2',
    brief: 'docs/gui/07-feature-briefs.md §15',
    protocol:
      'cmd download { track } / delete_download { video_id, delete_file }; `downloads` topic (per-video_id Running(pct)/Done/Failed)',
    seam: 'create gui/src/lib/stores/downloads.svelte.ts; wire the Downloads tab in LibraryView + the transport-bar ⬇ chip',
    capability: 'downloads-v8',
  },
  'queue.reorder': {
    title: 'Queue drag-reorder',
    milestone: 'M2',
    brief: 'docs/gui/07-feature-briefs.md §2 + docs/gui/05-frontend.md §7',
    protocol: 'cmd queue_move { from, to, expected_rev } (rev-guarded; stale_rev ⇒ snap back)',
    seam: 'pointer-events drag inside gui/src/lib/components/VirtualList.svelte + optimistic reorder in gui/src/lib/stores/queue.svelte.ts',
    notes:
      'Pointer events, NOT HTML5 DnD (unreliable in WKWebView). Auto-scroll during drag needs care.',
  },
  'settings.apply': {
    title: 'Settings read model + apply',
    milestone: 'M3',
    brief: 'docs/gui/07-feature-briefs.md §6–§7',
    protocol:
      'cmd apply { change: SettingChangeV8 } (grouped: playback/eq/streaming/ui/search/storage/scrobble); `settings` topic read model push',
    seam: 'create gui/src/lib/stores/settings.svelte.ts (read model + pending overlay + dirty tracking, docs/gui/05 §5.2); replace the wip() seams across gui/src/views/settings/*.svelte',
    capability: 'settings-v8',
    notes:
      'The tabs currently render TUI defaults so the forms are visually complete — swap them to the pushed model, keep the pending-overlay merge rule unit-tested.',
  },
  'settings.hotkeys': {
    title: 'Hotkeys (keymap model + capture + dispatcher)',
    milestone: 'M3',
    brief: 'docs/gui/07-feature-briefs.md §8 + docs/gui/05-frontend.md §8',
    protocol:
      'keymap read model (bindings + ActionInfo) in the `settings` topic; cmd apply { keymap: Bind/Unbind/ResetAll } — conflict detection stays core-side',
    seam: 'create gui/src/lib/keyboard/{chord,dispatcher,korean2set}.ts + ChordCapture.svelte; replace the provisional key handling in App.svelte and the wip() seams in HotkeysTab.svelte',
    capability: 'settings-v8',
    notes:
      'Korean IME 3-branch rule is normative (05 §8.4). Cross-check chord.ts against gui/src/generated/chord-fixtures.json once the Rust export lands.',
  },
  'settings.theme-editor': {
    title: 'Live theme editor (13 presets + 34 roles)',
    milestone: 'M3',
    brief: 'docs/gui/07-feature-briefs.md §9 + docs/gui/06-design-system.md §1–3',
    protocol:
      'cmd apply { theme: Preset/SetOverride/ClearOverride/BackgroundNone }; theme block of the `settings` push; preset preview palettes from the core',
    seam: 'extend gui/src/lib/stores/theme.svelte.ts (live push + oklab surface mix); replace the wip() seams in GraphicsTab.svelte',
    capability: 'settings-v8',
    notes:
      'Apply optimistically to the CSS var, reconcile on push (<100 ms target). The swatch rows already read live values from the CSS custom properties.',
  },
  'settings.animations': {
    title: 'Animation system (25 effects, two tickers)',
    milestone: 'M3',
    brief: 'docs/gui/06-design-system.md §5',
    protocol:
      'cmd apply { animations: Master/Fps/PauseUnfocused/Toggle }; animations block of the `settings` push',
    seam: 'create gui/src/lib/stores/anim.svelte.ts (shared rAF ticker, self-suspend contract); replace the wip() seams in GraphicsTab.svelte',
    capability: 'settings-v8',
    notes:
      'master off ⇒ cancelAnimationFrame outright, not a gated no-op. prefers-reduced-motion trumps config. Web equivalents land incrementally (most M5).',
  },
  'settings.accounts': {
    title: 'Accounts (Last.fm / ListenBrainz / Spotify)',
    milestone: 'M4',
    brief: 'docs/gui/07-feature-briefs.md §11 + docs/gui/02 §13.4',
    protocol:
      'cmd lastfm_connect { ticket } / spotify_connect { ticket } / listen_brainz_configure; `accounts` topic events (LastfmAuthUrl → LastfmConnected, …) — the GUI opens the browser via win:openUrl',
    seam: 'create gui/src/lib/stores/accounts.svelte.ts; replace the wip() seams in AccountsTab.svelte',
    capability: 'accounts-v8',
  },
  'ai.chat': {
    title: 'DJ Gem chat',
    milestone: 'M4',
    brief: 'docs/gui/07-feature-briefs.md §12',
    protocol:
      'cmd ask_ai { ticket, prompt }; `ai` topic (transcript appends, thinking flag, suggestions); suggestion play via play_tracks',
    seam: 'create gui/src/lib/stores/ai.svelte.ts; replace the wip() seams in gui/src/views/AiView.svelte',
    capability: 'ai',
    notes:
      'Disabled state has two flavors: ai_enabled off (enable CTA) vs capability missing on this owner.',
  },
  'ai.whygem': {
    title: 'Why-DJ-Gem popover',
    milestone: 'M4',
    brief: 'docs/gui/07-feature-briefs.md §13',
    protocol:
      'explanation per queue-item id (inlined in `queue` items or req fetch_why_gem { video_id })',
    seam: 'WhyGemPopover.svelte + the "why?" affordance on autoplay-added queue rows',
    capability: 'ai',
  },
  'transfer.wizard': {
    title: 'Spotify import wizard',
    milestone: 'M4',
    brief: 'docs/gui/07-feature-briefs.md §14',
    protocol:
      'cmd transfer_list_spotify { ticket } / transfer_start { ticket, spec } / transfer_cancel; `transfer` topic (job lifecycle + coalesced progress + report)',
    seam: 'create gui/src/lib/stores/transfer.svelte.ts + gui/src/views/wizards/SpotifyImport.svelte',
    capability: 'transfer-v8',
    notes:
      'Dest must surface YtmExistingPlaylist — dev-mode Spotify apps 403 on playlist creation since mid-2026, so append-to-existing is the mainline path, not an edge case.',
  },
  'radio.mode': {
    title: 'Radio mode switch',
    milestone: 'M5',
    brief: 'docs/gui/07-feature-briefs.md §16',
    protocol:
      'cmd apply { streaming: RadioMode { state } } — confirm modal is a frontend concern; the wire command is already-confirmed intent. Needs the capability on BOTH owners (daemon too).',
    seam: 'the Music/Radio switch in the App.svelte rail footer; Library radio tabs + Search radio default flip on player.radio_mode',
    capability: 'settings-v8',
  },
  'help.keymap': {
    title: 'Help overlay (live keymap)',
    milestone: 'M5',
    brief: 'docs/gui/07-feature-briefs.md §17',
    protocol:
      'keymap read model with per-(context, action) display labels — auto-generate the cheat sheet',
    seam: 'gui/src/views/overlays/HelpOverlay.svelte — swap DEFAULT_KEYMAP for the pushed model, add search',
    capability: 'settings-v8',
  },
  'lyrics.live': {
    title: 'Synced lyrics topic',
    milestone: 'B1',
    brief: 'docs/gui/02-remote-protocol-v8.md §7 (lyrics topic) + docs/gui/07 §1',
    protocol:
      '`lyrics` topic push — the demo shape { kind: "lyrics_snapshot", lines: [{ ms, text }] } is PROVISIONAL; align with the B1 wire and regenerate ts-rs types',
    seam: 'gui/src/lib/stores/lyrics.svelte.ts already consumes the provisional shape — reconcile it with the real PushEvent variant',
  },
  'artwork.live': {
    title: 'Artwork pipeline',
    milestone: 'B1',
    brief: 'docs/gui/02-remote-protocol-v8.md §12 + docs/gui/04 §art',
    protocol:
      '`artwork` topic + ytm://app/art/<key> custom-scheme serving (already implemented shell-side in src/desktop/assets.rs)',
    seam: 'gui/src/lib/components/AlbumArt.svelte already resolves ArtworkRef → URL; verify against a real core push, then delete the generated-placeholder fallback note',
  },
  'i18n.catalog': {
    title: 'i18n (en/ko catalog)',
    milestone: 'M5',
    brief: 'docs/gui/05-frontend.md §9',
    protocol:
      'frontend-owned keyed catalog; `settings` model carries language; live switch, no reload',
    seam: 'create gui/src/i18n/{en,ko}.json + i18n.svelte.ts; sweep every literal string in gui/src/views/** through t()',
    notes:
      'Seed with scripts/harvest-i18n.sh. Romanized titles stay core-side (display_* fields) — never romanize in the GUI.',
  },
};

export const FEATURE_IDS = Object.keys(WIRING) as FeatureId[];

/** The greppable marker for a feature's call sites. */
export function marker(id: FeatureId): string {
  return `wire:${WIRING[id].milestone}/${id}`;
}

/**
 * The copy-paste marching orders for the follow-up agent, generated from the registry so
 * the modal's "Copy agent brief" button always matches the spec. Plain markdown.
 */
export function agentBrief(id: FeatureId): string {
  const w = WIRING[id];
  const cap = w.capability
    ? `Capability gate: the UI auto-opens when the core advertises "${w.capability}" — advertise it from the session HelloAck when the server side lands.`
    : 'Capability gate: none — this seam is frontend-internal.';
  const notes = w.notes ? `Notes: ${w.notes}\n` : '';
  return `You are wiring a stubbed feature of the ytm-tui desktop GUI (gui/ — Svelte 5 runes + TypeScript, embedded into ytt-desktop).

Feature: ${w.title} [lands in ${w.milestone}]
Spec (read first): ${w.brief}; store contract in docs/gui/05-frontend.md §5.
Protocol: ${w.protocol}
Frontend seam: ${w.seam}
Call sites: rg "${marker(id)}" gui/src
${cap}
${notes}When the wire is live:
1. Replace every marked call site with the real client call (client.cmd/req/on — see gui/src/lib/ipc/client.ts).
2. Extend the browser demo core (gui/src/lib/dev/democore.ts) so \`npm run dev\` still exercises the feature.
3. Delete the '${id}' entry from gui/src/lib/wiring/registry.ts — gui/tests/wiring.test.ts fails if markers and registry drift.
4. Update gui/WIRING.md and docs/gui/PROGRESS.md.
Gates (run in gui/): npm run check && npm test && npm run build.`;
}
