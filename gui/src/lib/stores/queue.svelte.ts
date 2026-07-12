// Mirrors the `queue` topic: items in effective play order + the owner-global `rev`.
// The current row is DERIVED from playback.queue_pos — rows carry no current flag
// (docs/gui/02 §11.3). Membership mutations wait for the authoritative push: if admission is
// rejected, the visible queue never gets stranded in an optimistic state.
//
// LIVE-WIRED like playback: queue_snapshot pushes are the B0 wire; the mutation commands
// are v7 (queue_play/queue_remove) + v8 batch forms, including queue_move (drag-reorder).

import type { PushEvent } from '../../generated/protocol/PushEvent';
import type { TrackModel } from '../../generated/protocol/TrackModel';
import type { Client } from '../ipc/client';

export class QueueStore {
  items = $state<TrackModel[]>([]);
  rev = $state(0);
  readonly #client: Client;

  constructor(client: Client) {
    this.#client = client;
    client.on('queue', (payload) => this.#onPush(payload as PushEvent));
  }

  /** Jump to an order position (double-click a row). */
  play(position: number): void {
    this.#client.cmd('queue_play', { position });
  }

  remove(position: number): void {
    void this.#client.cmd('queue_remove_many', {
      positions: [position],
      expected_rev: this.rev,
    });
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

  #onPush(ev: PushEvent): void {
    if (ev.kind !== 'queue_snapshot') return;
    this.items = ev.model.items;
    this.rev = ev.model.rev;
  }
}
