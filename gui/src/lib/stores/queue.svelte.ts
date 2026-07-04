// Mirrors the `queue` topic: items in effective play order + the owner-global `rev`.
// The current row is DERIVED from playback.queue_pos — rows carry no current flag
// (docs/gui/02 §11.3). Membership mutations are optimistic; the next push snaps truth.
//
// LIVE-WIRED like playback: queue_snapshot pushes are the B0 wire; the mutation commands
// are v7 (queue_play/queue_remove) + v8 batch forms, including queue_move (drag-reorder).

import type { PushEvent } from '../../generated/protocol/PushEvent';
import type { TrackModel } from '../../generated/protocol/TrackModel';
import type { Client } from '../ipc/client';
import { applyMove } from '../dnd/reorder';

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
    this.items.splice(position, 1); // optimistic; rev-guarded server-side
    this.#client.cmd('queue_remove_many', { positions: [position], expected_rev: this.rev });
  }

  /** Drag-reorder: move the row at `from` to index `to`. Optimistic; rev-guarded server-side
   * (a stale_rev reject just means the next queue_snapshot snaps the order back). */
  move(from: number, to: number): void {
    if (from === to || from < 0 || from >= this.items.length) return;
    this.items = applyMove(this.items, from, to);
    this.#client.cmd('queue_move', { from, to, expected_rev: this.rev });
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
