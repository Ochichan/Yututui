// Mirrors the `queue` topic: items in effective play order + the owner-global `rev`.
// The current row is DERIVED from playback.queue_pos — rows carry no current flag
// (docs/gui/02 §11.3). Membership mutations wait for the authoritative push: if admission is
// rejected, the visible queue never gets stranded in an optimistic state.
//
// LIVE-WIRED like playback: queue_snapshot pushes are authoritative. Destructive row actions
// use revision-checked correlated requests so a stale UI can never target a different track.

import type { PushEvent } from '../../generated/protocol/PushEvent';
import type { TrackModel } from '../../generated/protocol/TrackModel';
import type { Client } from '../ipc/client';
import { t } from '../i18n.svelte';

export class QueueStore {
  items = $state<TrackModel[]>([]);
  rev = $state(0);
  readonly #client: Client;
  readonly #onError?: (message: string) => void;

  constructor(client: Client, onError?: (message: string) => void) {
    this.#client = client;
    this.#onError = onError;
    client.on('queue', (payload) => this.#onPush(payload as PushEvent));
  }

  /** Jump to an order position (double-click a row). */
  play(position: number): void {
    void this.#checked('queue_play_if_revision', position);
  }

  remove(position: number): void {
    void this.#checked('queue_remove_if_revision', position);
  }

  /** Drag-reorder: rev-guarded server-side; the next queue snapshot applies the move. */
  move(from: number, to: number): void {
    if (from === to || from < 0 || from >= this.items.length) return;
    void this.#client.cmd('queue_move', { from, to, expected_rev: this.rev });
  }

  /** GUI-new op (the TUI has no clear-upcoming): drop everything after `current`. */
  clearUpcoming(): void {
    this.#client.cmd('queue_clear_upcoming', { expected_rev: this.rev });
  }

  async #checked(name: string, position: number): Promise<void> {
    const expected_rev = this.rev;
    try {
      await this.#client.req(name, { position, expected_rev });
    } catch (error) {
      const reason = error instanceof Error ? error.message : String(error);
      this.#onError?.(
        reason === 'stale_rev' ? t('queue.stale') : t('queue.commandFailed', { reason }),
      );
      this.#client.refresh(['queue']);
    }
  }

  #onPush(ev: PushEvent): void {
    if (ev.kind !== 'queue_snapshot') return;
    this.items = ev.model.items;
    this.rev = ev.model.rev;
  }
}
