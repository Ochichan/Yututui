// Queue drag-reorder wiring (docs/gui/07 §2): the pure index/scroll math, the store's
// optimistic move + queue_move cmd, and the demo core's cursor-preserving reorder.

import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { applyMove, dropIndex, autoScrollStep } from '../src/lib/dnd/reorder';
import { Client } from '../src/lib/ipc/client';
import { QueueStore } from '../src/lib/stores/queue.svelte';
import { DemoCoreTransport } from '../src/lib/dev/democore';
import type { Transport } from '../src/lib/ipc/transport';
import type { InEnvelope, OutEnvelope } from '../src/lib/ipc/envelope';
import type { PlayerModel } from '../src/generated/protocol/PlayerModel';
import type { QueueModel } from '../src/generated/protocol/QueueModel';
import type { TrackModel } from '../src/generated/protocol/TrackModel';

class MockTransport implements Transport {
  readonly live = false;
  sent: OutEnvelope[] = [];
  #cb: ((env: InEnvelope) => void) | null = null;
  send(env: OutEnvelope): void {
    this.sent.push(env);
  }
  onMessage(cb: (env: InEnvelope) => void): void {
    this.#cb = cb;
  }
  emit(env: InEnvelope): void {
    this.#cb?.(env);
  }
}

const stub = (id: string) => ({ video_id: id, title: id }) as unknown as TrackModel;

describe('reorder math', () => {
  it('dropIndex clamps to the valid slot range', () => {
    expect(dropIndex(0, 52, 5)).toBe(0);
    expect(dropIndex(130, 52, 5)).toBe(2); // floor(130/52) = 2
    expect(dropIndex(-40, 52, 5)).toBe(0);
    expect(dropIndex(9999, 52, 5)).toBe(4);
    expect(dropIndex(10, 52, 0)).toBe(0);
  });

  it('applyMove relocates without mutating the source and no-ops out of range', () => {
    const src = ['a', 'b', 'c', 'd'];
    expect(applyMove(src, 0, 2)).toEqual(['b', 'c', 'a', 'd']);
    expect(applyMove(src, 3, 1)).toEqual(['a', 'd', 'b', 'c']);
    expect(applyMove(src, 1, 1)).toEqual(['a', 'b', 'c', 'd']);
    expect(applyMove(src, 9, 0)).toEqual(['a', 'b', 'c', 'd']);
    expect(src).toEqual(['a', 'b', 'c', 'd']); // untouched
  });

  it('autoScrollStep is signed near the edges and zero in the middle', () => {
    expect(autoScrollStep(200, 400)).toBe(0);
    expect(autoScrollStep(5, 400)).toBeLessThan(0);
    expect(autoScrollStep(398, 400)).toBeGreaterThan(0);
  });
});

describe('QueueStore.move', () => {
  it('sends queue_move with the current rev and waits for an authoritative snapshot', () => {
    const t = new MockTransport();
    const store = new QueueStore(new Client(t));
    t.emit({
      v: 1,
      kind: 'event',
      topic: 'queue',
      payload: {
        kind: 'queue_snapshot',
        model: { rev: 7, items: [stub('a'), stub('b'), stub('c')] } as QueueModel,
      },
    });
    store.move(0, 2);
    expect(store.items.map((x) => x.video_id)).toEqual(['a', 'b', 'c']);
    const sent = t.sent.at(-1)!;
    expect(sent).toMatchObject({ kind: 'cmd', name: 'queue_move' });
    expect(sent.payload).toEqual({ from: 0, to: 2, expected_rev: 7 });
  });

  it('ignores a no-op or out-of-range move', () => {
    const t = new MockTransport();
    const store = new QueueStore(new Client(t));
    t.emit({
      v: 1,
      kind: 'event',
      topic: 'queue',
      payload: { kind: 'queue_snapshot', model: { rev: 1, items: [stub('a')] } as QueueModel },
    });
    store.move(0, 0);
    store.move(5, 0);
    expect(t.sent.length).toBe(0);
  });
});

describe('demo core queue_move', () => {
  beforeEach(() => vi.useFakeTimers());
  afterEach(() => vi.useRealTimers());

  function boot() {
    const t = new DemoCoreTransport();
    const frames: InEnvelope[] = [];
    t.onMessage((e) => frames.push(e));
    vi.advanceTimersByTime(200);
    t.send({ v: 1, kind: 'sub', name: 'subscribe', payload: ['player', 'queue'] });
    vi.advanceTimersByTime(50);
    return { t, frames };
  }
  const lastQueue = (frames: InEnvelope[]) =>
    (
      [...frames].reverse().find((e) => e.kind === 'event' && e.topic === 'queue')!.payload as {
        model: QueueModel;
      }
    ).model;
  const lastPlayer = (frames: InEnvelope[]) =>
    (
      [...frames].reverse().find((e) => e.kind === 'event' && e.topic === 'player')!.payload as {
        model: PlayerModel;
      }
    ).model;

  it('reorders the queue, bumps rev, and keeps the cursor on the same track', () => {
    const { t, frames } = boot();
    const before = lastQueue(frames);
    const firstId = before.items[0].video_id; // the current track (pos 0)

    // Move the current track from 0 to 2; the cursor should follow it to index 2.
    t.send({ v: 1, kind: 'cmd', name: 'queue_move', payload: { from: 0, to: 2, expected_rev: 1 } });
    vi.advanceTimersByTime(20);

    const after = lastQueue(frames);
    expect(after.items[2].video_id).toBe(firstId);
    expect(after.rev).toBeGreaterThan(before.rev);
    expect(lastPlayer(frames).queue_pos).toBe(2);
    expect(lastPlayer(frames).track?.video_id).toBe(firstId);
  });

  it('a move that does not touch the cursor leaves queue_pos put', () => {
    const { t, frames } = boot();
    // Cursor at 0; moving 2→4 shouldn't shift it.
    t.send({ v: 1, kind: 'cmd', name: 'queue_move', payload: { from: 2, to: 4, expected_rev: 1 } });
    vi.advanceTimersByTime(20);
    expect(lastPlayer(frames).queue_pos).toBe(0);
  });
});
