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

export type FeatureId = 'core.v8-commands';

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
  'core.v8-commands': {
    title: 'Deferred v8 core commands',
    milestone: 'B2',
    brief: 'docs/gui/02-remote-protocol-v8.md §13 (command surface) + gui/WIRING.md §Deferred',
    protocol:
      'The frozen src/remote/proto/command.rs still lacks the v8 command variants the GUI already speaks: rate, queue_move, queue_clear_upcoming, play_video, ask_ai, library_play/enqueue/remove, fetch_library_page, download, delete_download, keymap_unbind, keymap_reset_all, lastfm_connect, spotify_connect, listen_brainz_configure, account_set, transfer_*, playlist_*, fetch_playlist_detail, fetch_why_gem — the desktop gateway answers bad_command for each (src/desktop/gateway.rs) until the variants exist',
    seam: 'The stores already send the final shapes (playback/queue/ai/library/downloads/keymap/accounts/transfer/playlists) and the demo core answers all of them, so demo mode is always wired. Main-screen entry points (rate, video, queue reorder/clear, AI composer) gate through wip.gate; deeper surfaces (playlists CRUD, accounts connect, transfer wizard, hotkey editing, downloads) still surface the bad_command toast until the variants land',
    capability: 'v8-commands',
    notes:
      'Server-side work first: add the RemoteCommand variants + core dispatch + parity tests (lockstep), regenerate ts-rs types, then advertise the capability — the gates dissolve without a frontend release.',
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
  return `You are wiring a stubbed feature of the yututui desktop GUI (gui/ — Svelte 5 runes + TypeScript, embedded into yututray).

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
