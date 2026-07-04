// The demo core must speak real envelopes and real protocol shapes — it is the fixture
// vehicle for browser dev and E2E (docs/gui/05 §4.3), so its behavior is pinned.

import { beforeEach, afterEach, describe, expect, it, vi } from 'vitest';
import { DemoCoreTransport } from '../src/lib/dev/democore';
import type { InEnvelope } from '../src/lib/ipc/envelope';
import type { PlayerModel } from '../src/generated/protocol/PlayerModel';
import type { QueueModel } from '../src/generated/protocol/QueueModel';

interface Harness {
  t: DemoCoreTransport;
  frames: InEnvelope[];
}

function boot(): Harness {
  const t = new DemoCoreTransport();
  const frames: InEnvelope[] = [];
  t.onMessage((env) => frames.push(env));
  vi.advanceTimersByTime(200); // connecting → online
  t.send({ v: 1, kind: 'sub', name: 'subscribe', payload: ['player', 'queue', 'lyrics'] });
  vi.advanceTimersByTime(50);
  return { t, frames };
}

function lastPlayer(frames: InEnvelope[]): PlayerModel {
  const f = [...frames].reverse().find((e) => e.kind === 'event' && e.topic === 'player');
  return (f!.payload as { model: PlayerModel }).model;
}

function lastQueue(frames: InEnvelope[]): QueueModel {
  const f = [...frames].reverse().find((e) => e.kind === 'event' && e.topic === 'queue');
  return (f!.payload as { model: QueueModel }).model;
}

describe('DemoCoreTransport', () => {
  beforeEach(() => vi.useFakeTimers());
  afterEach(() => vi.useRealTimers());

  it('reports online and answers subscribe with snapshots', () => {
    const { frames } = boot();
    const conn = frames.find(
      (f) => f.kind === 'conn' && (f.payload as { state: string }).state === 'online',
    );
    expect(conn).toBeTruthy();
    expect(lastPlayer(frames).track).not.toBeNull();
    expect(lastQueue(frames).items.length).toBeGreaterThan(3);
  });

  it('toggle_pause flips paused and pushes a player snapshot', () => {
    const { t, frames } = boot();
    const before = lastPlayer(frames).paused;
    t.send({ v: 1, kind: 'cmd', name: 'toggle_pause' });
    vi.advanceTimersByTime(50);
    expect(lastPlayer(frames).paused).toBe(!before);
  });

  it('queue_play jumps the cursor and unpauses', () => {
    const { t, frames } = boot();
    t.send({ v: 1, kind: 'cmd', name: 'queue_play', payload: { position: 3 } });
    vi.advanceTimersByTime(50);
    const p = lastPlayer(frames);
    expect(p.queue_pos).toBe(3);
    expect(p.paused).toBe(false);
    expect(p.elapsed_ms).toBe(0);
  });

  it('queue_remove_many drops rows and bumps rev', () => {
    const { t, frames } = boot();
    const before = lastQueue(frames);
    t.send({
      v: 1,
      kind: 'cmd',
      name: 'queue_remove_many',
      payload: { positions: [1, 2], expected_rev: before.rev },
    });
    vi.advanceTimersByTime(50);
    const after = lastQueue(frames);
    expect(after.items.length).toBe(before.items.length - 2);
    expect(after.rev).toBeGreaterThan(before.rev);
  });

  it('req ping answers res pong', () => {
    const { t, frames } = boot();
    t.send({ v: 1, kind: 'req', id: 7, name: 'ping' });
    vi.advanceTimersByTime(50);
    const res = frames.find((f) => f.kind === 'res' && f.id === 7);
    expect(res?.payload).toContain('pong');
  });

  it('unknown req is rejected with not_supported (reason-coded, like the real core)', () => {
    const { t, frames } = boot();
    // A request the demo core doesn't answer (fetch_playlist_detail / fetch_why_gem are
    // both wired now; this stands in for any not-yet-wired req).
    t.send({ v: 1, kind: 'req', id: 8, name: 'fetch_nonexistent' });
    vi.advanceTimersByTime(50);
    const err = frames.find((f) => f.kind === 'err' && f.id === 8);
    expect((err?.payload as { reason: string }).reason).toBe('not_supported');
  });
});
