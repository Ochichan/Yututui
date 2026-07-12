// Search wiring (docs/gui/07 §3): the store's ticket discipline (drop stale completions)
// and the demo core's run_search / play_tracks / enqueue_tracks behavior, so `npm run dev`
// and the real M2 wire share one contract.

import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { Client } from '../src/lib/ipc/client';
import { SearchStore, type SearchCompleted } from '../src/lib/stores/search.svelte';
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
  ticketOf(name: string): number {
    const e = [...this.sent].reverse().find((s) => s.name === name);
    if (!e) throw new Error(`no ${name} sent`);
    return (e.payload as { ticket: number }).ticket;
  }
}

function completion(ticket: number, query: string): SearchCompleted {
  return {
    kind: 'search_completed',
    ticket,
    query,
    source: 'youtube',
    groups: [{ source: 'youtube', tracks: [], error: null }],
  };
}

function track(videoId: string): TrackModel {
  return {
    video_id: videoId,
    title: 'External row',
    artist: 'Provider artist',
    album: null,
    duration_ms: 120_000,
    source: 'jamendo',
    is_local: false,
    downloaded: false,
    favorite: false,
    disliked: false,
    display_title: null,
    display_artist: null,
    artwork: null,
    watch_url: null,
    is_live: false,
  };
}

describe('SearchStore', () => {
  it('applies the completion whose ticket matches the latest query', () => {
    const t = new MockTransport();
    const store = new SearchStore(new Client(t));
    store.run('cats', 'youtube');
    expect(store.pending).toBe(true);
    expect(store.ran).toBe(true);
    t.emit({
      v: 1,
      kind: 'event',
      topic: 'search',
      payload: completion(t.ticketOf('run_search'), 'cats'),
    });
    expect(store.pending).toBe(false);
    expect(store.query).toBe('cats');
  });

  it('drops a stale completion from a superseded query', () => {
    const t = new MockTransport();
    const store = new SearchStore(new Client(t));
    store.run('first', 'youtube');
    const stale = t.ticketOf('run_search');
    store.run('second', 'youtube'); // supersedes — the ticket advances
    const fresh = t.ticketOf('run_search');
    expect(fresh).toBeGreaterThan(stale);

    // The late reply to the FIRST query must not land over the second.
    t.emit({ v: 1, kind: 'event', topic: 'search', payload: completion(stale, 'first') });
    expect(store.pending).toBe(true);
    expect(store.query).toBe('second');

    t.emit({ v: 1, kind: 'event', topic: 'search', payload: completion(fresh, 'second') });
    expect(store.pending).toBe(false);
  });

  it('ignores a blank query', () => {
    const t = new MockTransport();
    const store = new SearchStore(new Client(t));
    store.run('   ', 'youtube');
    expect(t.sent.length).toBe(0);
    expect(store.ran).toBe(false);
  });

  it('clears the pending spinner when search admission is rejected', async () => {
    const t = new MockTransport();
    const store = new SearchStore(new Client(t));
    store.run('cats', 'youtube');
    const command = t.sent.at(-1)!;
    t.emit({ v: 1, id: command.id, kind: 'err', payload: { reason: 'offline' } });
    await vi.waitFor(() => expect(store.pending).toBe(false));
  });

  it('retires external rows across disconnect and rejects their late completion', () => {
    const t = new MockTransport();
    const store = new SearchStore(new Client(t));
    store.run('provider row', 'jamendo');
    const ticket = t.ticketOf('run_search');
    const result = completion(ticket, 'provider row');
    result.source = 'jamendo';
    result.groups = [{ source: 'jamendo', tracks: [track('gui:jamendo:stale')], error: null }];

    t.emit({ v: 1, kind: 'event', topic: 'search', payload: result });
    expect(store.total).toBe(1);

    t.emit({ v: 1, kind: 'conn', payload: { state: 'offline', reason: 'disconnected' } });
    t.emit({ v: 1, kind: 'conn', payload: { state: 'online' } });
    t.emit({ v: 1, kind: 'event', topic: 'search', payload: result });

    expect(store.pending).toBe(false);
    expect(store.total).toBe(0);
    expect(store.groups).toEqual([]);
  });
});

describe('demo core search', () => {
  beforeEach(() => vi.useFakeTimers());
  afterEach(() => vi.useRealTimers());

  function boot() {
    const t = new DemoCoreTransport();
    const frames: InEnvelope[] = [];
    t.onMessage((e) => frames.push(e));
    vi.advanceTimersByTime(200); // connecting → online
    t.send({ v: 1, kind: 'sub', name: 'subscribe', payload: ['player', 'queue', 'search'] });
    vi.advanceTimersByTime(50);
    return { t, frames };
  }
  const lastSearch = (frames: InEnvelope[]) =>
    [...frames].reverse().find((e) => e.kind === 'event' && e.topic === 'search')!
      .payload as SearchCompleted;
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

  it('run_search over all catalogs returns groups incl. the Jamendo error chip', () => {
    const { t, frames } = boot();
    t.send({
      v: 1,
      kind: 'cmd',
      name: 'run_search',
      payload: { ticket: 1, query: 'purrple', source: 'all' },
    });
    vi.advanceTimersByTime(50);
    const res = lastSearch(frames);
    expect(res.kind).toBe('search_completed');
    expect(res.ticket).toBe(1);
    const jamendo = res.groups.find((g) => g.source === 'jamendo')!;
    expect(jamendo.error).toBeTruthy();
    expect(jamendo.tracks.length).toBe(0);
    expect(res.groups.find((g) => g.source === 'youtube')!.tracks.length).toBeGreaterThan(0);
  });

  it('play_tracks inserts a search result and makes it the current track', () => {
    const { t, frames } = boot();
    const before = lastQueue(frames).items.length;
    t.send({
      v: 1,
      kind: 'cmd',
      name: 'run_search',
      payload: { ticket: 1, query: 'tailwind', source: 'youtube' },
    });
    vi.advanceTimersByTime(50);
    const pick = lastSearch(frames).groups[0].tracks[0];
    t.send({ v: 1, kind: 'cmd', name: 'play_tracks', payload: { video_ids: [pick.video_id] } });
    vi.advanceTimersByTime(50);
    expect(lastQueue(frames).items.length).toBe(before + 1);
    expect(lastPlayer(frames).track?.video_id).toBe(pick.video_id);
  });

  it('enqueue_tracks appends without changing the current track', () => {
    const { t, frames } = boot();
    t.send({
      v: 1,
      kind: 'cmd',
      name: 'run_search',
      payload: { ticket: 1, query: 'nine', source: 'youtube' },
    });
    vi.advanceTimersByTime(50);
    const pick = lastSearch(frames).groups[0].tracks[0];
    const beforeLen = lastQueue(frames).items.length;
    const curBefore = lastPlayer(frames).track?.video_id;
    t.send({ v: 1, kind: 'cmd', name: 'enqueue_tracks', payload: { video_ids: [pick.video_id] } });
    vi.advanceTimersByTime(50);
    expect(lastQueue(frames).items.length).toBe(beforeLen + 1);
    expect(lastPlayer(frames).track?.video_id).toBe(curBefore);
  });
});
