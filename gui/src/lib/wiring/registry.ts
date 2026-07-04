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
  'queue.reorder' | 'ai.whygem' | 'lyrics.live' | 'artwork.live' | 'i18n.catalog';

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
  'queue.reorder': {
    title: 'Queue drag-reorder',
    milestone: 'M2',
    brief: 'docs/gui/07-feature-briefs.md §2 + docs/gui/05-frontend.md §7',
    protocol: 'cmd queue_move { from, to, expected_rev } (rev-guarded; stale_rev ⇒ snap back)',
    seam: 'pointer-events drag inside gui/src/lib/components/VirtualList.svelte + optimistic reorder in gui/src/lib/stores/queue.svelte.ts',
    notes:
      'Pointer events, NOT HTML5 DnD (unreliable in WKWebView). Auto-scroll during drag needs care.',
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
