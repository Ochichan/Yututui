// Why-DJ-Gem popover (docs/gui/07 §13): the pick explanation for an autoplay-added queue
// row. Two provisional wire halves, both spec-sanctioned:
//   • provenance — which rows are DJ-Gem picks, so the UI knows where to hang the "why?"
//     affordance — rides the `ai` topic as `{ kind: 'why_gem_provenance', video_ids }`
//     (AiStore filters on kind, so both stores share the one subscription);
//   • the explanation itself — fetched on open with `req fetch_why_gem { video_id }`
//     (the frozen TrackModel carries no explanation field).
// Both shapes live only in the demo core today — reconcile with the M4 core wire + ts-rs
// types when they land (mirrors the search/library/ai provisional note; see gui/WIRING.md).

import type { Client } from '../ipc/client';

/** The pick rationale, matching the TUI overlay: slot role + reason codes + confidence. */
export interface WhyGem {
  /** The autoplay slot this pick filled, e.g. "Deep cut" / "More like this". */
  slot: string;
  /** Reason codes / phrases the model attached to the pick. */
  reasons: string[];
  /** Model confidence, `0..1`; null for provenance-only picks (no model score). */
  confidence: number | null;
}

// PROVISIONAL `ai` topic sibling event — only the demo core speaks it.
interface WhyGemProvenance {
  kind: 'why_gem_provenance';
  video_ids: string[];
}

export class WhyGemStore {
  /** Queue rows the DJ Gem autoplay added — the rows that get a "why?" affordance. */
  provenance = $state<Set<string>>(new Set());
  /** The row whose popover is open, or null. */
  openId = $state<string | null>(null);
  /** Viewport anchor for the popover (from the affordance's click). */
  anchor = $state<{ x: number; y: number } | null>(null);
  /** The fetched explanation for `openId`, or null while loading / on failure. */
  detail = $state<WhyGem | null>(null);
  loading = $state(false);
  #seq = 0;
  readonly #client: Client;

  constructor(client: Client) {
    this.#client = client;
    this.#client.on('ai', (payload) => this.#onPush(payload as { kind?: string }));
  }

  /** Does this queue row carry a gem explanation (⇒ show the affordance)? */
  has(videoId: string): boolean {
    return this.provenance.has(videoId);
  }

  /** Open the popover for a row and fetch its explanation (drop-stale on the seq guard). */
  async open(videoId: string, anchor: { x: number; y: number }): Promise<void> {
    this.openId = videoId;
    this.anchor = anchor;
    this.detail = null;
    this.loading = true;
    const seq = ++this.#seq;
    try {
      const res = await this.#client.req<WhyGem | null>('fetch_why_gem', { video_id: videoId });
      if (seq !== this.#seq) return; // a newer open / close superseded us
      this.detail = res ?? null;
    } catch {
      if (seq === this.#seq) this.detail = null;
    } finally {
      if (seq === this.#seq) this.loading = false;
    }
  }

  close(): void {
    this.openId = null;
    this.anchor = null;
    this.detail = null;
    this.loading = false;
    this.#seq++; // invalidate any in-flight fetch so it can't paint after close
  }

  #onPush(payload: { kind?: string }): void {
    if (payload.kind !== 'why_gem_provenance') return;
    this.provenance = new Set((payload as WhyGemProvenance).video_ids ?? []);
    // If the open row lost its gem status (e.g. queue swap), fold the popover.
    if (this.openId && !this.provenance.has(this.openId)) this.close();
  }
}
