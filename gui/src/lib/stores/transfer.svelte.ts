// Spotify import wizard (docs/gui/07 §14): list a user's Spotify playlists, pick a
// destination, then run a coalesced-progress import that ends in a match report. The whole
// job lives on the `transfer` topic — the store just mirrors the pushed state machine and
// sends the three commands. The destination surfaces an existing YTM playlist as a
// first-class option: dev-mode Spotify apps 403 on playlist creation since mid-2026, so
// append-to-existing is the mainline path (the wizard reads ctx.playlists.list for it). The
// desktop bridge forwards these once the core advertises `transfer-v8`; until then the
// in-page demo core answers (see gui/WIRING.md).

import type { Client } from '../ipc/client';

export type TransferPhase = 'idle' | 'listing' | 'ready' | 'running' | 'done' | 'failed';

// PROVISIONAL wire shapes — only the demo core speaks them. Reconcile with the M4 core wire +
// ts-rs types when they land.
export interface SpotifyPlaylist {
  id: string;
  name: string;
  count: number;
}
export interface TransferJob {
  done: number;
  total: number;
  matched: number;
  failed: number;
}
export interface TransferReport {
  matched: number;
  failed: number;
  skipped: number;
  /** Titles that found no YTM match — surfaced so the user can chase them by hand. */
  unmatched: string[];
  /** Human label of where the tracks landed. */
  dest: string;
}
export interface TransferState {
  kind: 'transfer_state';
  phase: TransferPhase;
  sources: SpotifyPlaylist[];
  job: TransferJob | null;
  report: TransferReport | null;
  error: string | null;
}

/** Where imported tracks land: a new YTM playlist, or an existing one (the mainline path). */
export type TransferDest =
  { kind: 'new'; name: string } | { kind: 'existing'; playlist_id: string };

export interface TransferSpec {
  source_ids: string[];
  dest: TransferDest;
}

function idle(): TransferState {
  return {
    kind: 'transfer_state',
    phase: 'idle',
    sources: [],
    job: null,
    report: null,
    error: null,
  };
}

export class TransferStore {
  state = $state<TransferState>(idle());
  readonly #client: Client;

  constructor(client: Client) {
    this.#client = client;
    this.#client.on('transfer', (payload) => this.#onPush(payload as TransferState));
  }

  get phase(): TransferPhase {
    return this.state.phase;
  }

  /** Kick the connect-and-list flow; sources arrive via the topic (listing → ready). */
  listSpotify(): void {
    this.#client.cmd('transfer_list_spotify', {});
  }

  start(spec: TransferSpec): void {
    if (spec.source_ids.length === 0) return;
    this.#client.cmd('transfer_start', { spec });
  }

  cancel(): void {
    this.#client.cmd('transfer_cancel', {});
  }

  /** Client-only reset so reopening the wizard starts clean (topic is authoritative once a
   * job runs). */
  reset(): void {
    this.state = idle();
  }

  #onPush(s: TransferState): void {
    if (s.kind !== 'transfer_state') return;
    this.state = s;
  }
}
