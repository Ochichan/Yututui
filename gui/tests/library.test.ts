// Library wiring (docs/gui/07 §4): the store's request sequencing (paging + drop a stale
// page when the scope changes, re-fetch on invalidation) and the demo core's
// fetch_library_page / library_play / library_remove behavior.

import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { Client } from '../src/lib/ipc/client';
import { LibraryStore, type LibraryPage } from '../src/lib/stores/library.svelte';
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
  lastReq(name: string): OutEnvelope {
    const e = [...this.sent].reverse().find((s) => s.kind === 'req' && s.name === name);
    if (!e) throw new Error(`no ${name} req`);
    return e;
  }
  reqCount(name: string): number {
    return this.sent.filter((s) => s.kind === 'req' && s.name === name).length;
  }
}

const stub = (id: string) => ({ video_id: id, title: id }) as unknown as TrackModel;
function page(scope: string, count: number, total: number, offset = 0): LibraryPage {
  return {
    scope: scope as LibraryPage['scope'],
    filter: '',
    offset,
    total,
    tracks: Array.from({ length: count }, (_, i) => stub(`${scope}-${offset + i}`)),
  };
}

describe('LibraryStore', () => {
  it('resolves the first page and reports hasMore', async () => {
    const t = new MockTransport();
    const store = new LibraryStore(new Client(t));
    const done = store.load('all', '');
    const req = t.lastReq('fetch_library_page');
    expect(req.payload).toMatchObject({ scope: 'all', offset: 0 });
    t.emit({ v: 1, id: req.id, kind: 'res', payload: page('all', 50, 120) });
    await done;
    expect(store.tracks.length).toBe(50);
    expect(store.total).toBe(120);
    expect(store.hasMore).toBe(true);
    expect(store.loading).toBe(false);
  });

  it('more() appends the next window', async () => {
    const t = new MockTransport();
    const store = new LibraryStore(new Client(t));
    const first = store.load('all', '');
    t.emit({ v: 1, id: t.lastReq('fetch_library_page').id, kind: 'res', payload: page('all', 50, 120) });
    await first;
    const next = store.more();
    const req = t.lastReq('fetch_library_page');
    expect(req.payload).toMatchObject({ offset: 50 });
    t.emit({ v: 1, id: req.id, kind: 'res', payload: page('all', 50, 120, 50) });
    await next;
    expect(store.tracks.length).toBe(100);
    expect(store.hasMore).toBe(true);
  });

  it('drops a stale page when the scope changes mid-flight', async () => {
    const t = new MockTransport();
    const store = new LibraryStore(new Client(t));
    const first = store.load('all', '');
    const firstReq = t.lastReq('fetch_library_page');
    const second = store.load('favorites', '');
    const secondReq = t.lastReq('fetch_library_page');

    // The late reply to the superseded 'all' load must not land over 'favorites'.
    t.emit({ v: 1, id: firstReq.id, kind: 'res', payload: page('all', 50, 120) });
    t.emit({ v: 1, id: secondReq.id, kind: 'res', payload: page('favorites', 3, 3) });
    await Promise.all([first, second]);

    expect(store.scope).toBe('favorites');
    expect(store.total).toBe(3);
    expect(store.tracks.length).toBe(3);
  });

  it('re-fetches the current scope on a library invalidation', async () => {
    const t = new MockTransport();
    const store = new LibraryStore(new Client(t));
    const first = store.load('favorites', '');
    t.emit({ v: 1, id: t.lastReq('fetch_library_page').id, kind: 'res', payload: page('favorites', 2, 2) });
    await first;

    const before = t.reqCount('fetch_library_page');
    t.emit({ v: 1, kind: 'event', topic: 'library', payload: { kind: 'library_invalidated' } });
    expect(t.reqCount('fetch_library_page')).toBe(before + 1);
    expect(t.lastReq('fetch_library_page').payload).toMatchObject({ scope: 'favorites' });
  });
});

describe('demo core library', () => {
  beforeEach(() => vi.useFakeTimers());
  afterEach(() => vi.useRealTimers());

  function boot() {
    const t = new DemoCoreTransport();
    const frames: InEnvelope[] = [];
    t.onMessage((e) => frames.push(e));
    vi.advanceTimersByTime(200);
    t.send({ v: 1, kind: 'sub', name: 'subscribe', payload: ['player', 'queue', 'library'] });
    vi.advanceTimersByTime(50);
    return { t, frames };
  }
  const resById = (frames: InEnvelope[], id: number) =>
    [...frames].reverse().find((e) => e.kind === 'res' && e.id === id)!.payload as LibraryPage;
  const lastQueue = (frames: InEnvelope[]) =>
    ([...frames].reverse().find((e) => e.kind === 'event' && e.topic === 'queue')!.payload as {
      model: QueueModel;
    }).model;
  const lastPlayer = (frames: InEnvelope[]) =>
    ([...frames].reverse().find((e) => e.kind === 'event' && e.topic === 'player')!.payload as {
      model: PlayerModel;
    }).model;

  it('fetch_library_page returns the whole scope, filter narrows it', () => {
    const { t, frames } = boot();
    t.send({
      v: 1,
      id: 1,
      kind: 'req',
      name: 'fetch_library_page',
      payload: { scope: 'all', filter: '', offset: 0, limit: 50 },
    });
    vi.advanceTimersByTime(50);
    expect(resById(frames, 1).total).toBe(10); // the demo catalog

    t.send({
      v: 1,
      id: 2,
      kind: 'req',
      name: 'fetch_library_page',
      payload: { scope: 'all', filter: 'purrple', offset: 0, limit: 50 },
    });
    vi.advanceTimersByTime(50);
    expect(resById(frames, 2).total).toBe(1);
  });

  it('library_play replaces the queue with the scope and starts it', () => {
    const { t, frames } = boot();
    t.send({ v: 1, kind: 'cmd', name: 'library_play', payload: { scope: 'favorites', filter: '' } });
    vi.advanceTimersByTime(50);
    // The demo catalog has two favorites (demo-001, demo-007).
    expect(lastQueue(frames).items.length).toBe(2);
    expect(lastPlayer(frames).track?.favorite).toBe(true);
  });

  it('library_remove drops the row and pushes an invalidation', () => {
    const { t, frames } = boot();
    t.send({
      v: 1,
      kind: 'cmd',
      name: 'library_remove',
      payload: { scope: 'favorites', video_id: 'demo-001' },
    });
    vi.advanceTimersByTime(50);
    expect(frames.some((e) => e.kind === 'event' && e.topic === 'library')).toBe(true);

    t.send({
      v: 1,
      id: 9,
      kind: 'req',
      name: 'fetch_library_page',
      payload: { scope: 'favorites', filter: '', offset: 0, limit: 50 },
    });
    vi.advanceTimersByTime(50);
    expect(resById(frames, 9).total).toBe(1); // demo-007 only, demo-001 removed
  });
});
