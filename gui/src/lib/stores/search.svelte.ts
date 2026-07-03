// Search (docs/gui/07 §3): ticketed async query across the six catalogs. Each `run` bumps a
// monotonic ticket and sends `run_search`; the core answers with a `search` topic push whose
// ticket must match the latest — stale completions from a superseded query are dropped, so a
// fast typist never sees an old result land over a new one. Play/enqueue send the v8
// track-list commands (the desktop bridge forwards them once the core advertises `search-v8`;
// until then the in-page demo core answers — see gui/WIRING.md).

import type { SearchSource } from '../../generated/protocol/SearchSource';
import type { TrackModel } from '../../generated/protocol/TrackModel';
import type { Client } from '../ipc/client';

// PROVISIONAL wire shape — only the demo core speaks it today. Reconcile with the real
// `search` topic + ts-rs types when M2 core lands (mirrors the lyrics.live provisional note).
export interface SearchGroup {
  /** A concrete catalog, never `'all'`. */
  source: SearchSource;
  tracks: TrackModel[];
  /** Per-source failure surfaced as a chip (e.g. "Jamendo: no client id"); null on success. */
  error: string | null;
}
export interface SearchCompleted {
  kind: 'search_completed';
  ticket: number;
  query: string;
  /** The requested scope — `'all'` or a single catalog. */
  source: SearchSource;
  groups: SearchGroup[];
}

export class SearchStore {
  groups = $state<SearchGroup[]>([]);
  pending = $state(false);
  /** The query behind the currently displayed / in-flight results. */
  query = $state('');
  /** True once a search has run this session (drives the empty-state copy). */
  ran = $state(false);
  #ticket = 0;
  readonly #client: Client;

  constructor(client: Client) {
    this.#client = client;
    client.on('search', (payload) => this.#onPush(payload as SearchCompleted));
  }

  /** Run a search. No-op on a blank query (the input already guards, but be defensive). */
  run(query: string, source: SearchSource): void {
    const q = query.trim();
    if (!q) return;
    this.#ticket += 1;
    this.query = q;
    this.ran = true;
    this.pending = true;
    this.groups = [];
    this.#client.cmd('run_search', { ticket: this.#ticket, query: q, source });
  }

  /** Play a result now (replaces nothing — inserts and jumps, core-defined). */
  play(track: TrackModel): void {
    this.#client.cmd('play_tracks', { video_ids: [track.video_id] });
  }

  /** Append a result to the queue. */
  enqueue(track: TrackModel): void {
    this.#client.cmd('enqueue_tracks', { video_ids: [track.video_id] });
  }

  /** Total tracks across every source group. */
  get total(): number {
    return this.groups.reduce((n, g) => n + g.tracks.length, 0);
  }

  /** True when the last completed search returned nothing playable (errors aside). */
  get empty(): boolean {
    return this.ran && !this.pending && this.total === 0;
  }

  #onPush(ev: SearchCompleted): void {
    if (ev.kind !== 'search_completed') return;
    if (ev.ticket !== this.#ticket) return; // a newer query superseded this one — drop it
    this.query = ev.query;
    this.groups = ev.groups;
    this.pending = false;
  }
}
