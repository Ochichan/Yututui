// Mirrors the `queue` topic: items in effective play order + the owner-global `rev`.
// The current row is DERIVED from playback.queue_pos — rows carry no current flag
// (docs/gui/02 §11.3). Membership mutations are optimistic; the next push snaps truth.
//
// LIVE-WIRED like playback: queue_snapshot pushes are the B0 wire; the mutation commands
// are v7 (queue_play/queue_remove) + v8 batch forms. Drag-reorder stays behind the
// TODO(wire:M2/queue.reorder) seam in QueuePanel.svelte.

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
    this.items.splice(position, 1); // optimistic; rev-guarded server-side
    this.#client.cmd('queue_remove_many', { positions: [position], expected_rev: this.rev });
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
