// Playlists wiring (docs/gui/07 §5): the store's CRUD commands + drill-down request
// (with a stale-fetch guard and detail reconcile on a topic push), and the demo core's
// playlists_snapshot / fetch_playlist_detail / create / delete / add / remove / play.

import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { Client } from '../src/lib/ipc/client';
import {
  PlaylistsStore,
  type PlaylistDetail,
  type PlaylistSummary,
  type PlaylistsSnapshot,
} from '../src/lib/stores/playlists.svelte';
import { DemoCoreTransport } from '../src/lib/dev/democore';
import type { Transport } from '../src/lib/ipc/transport';
import type { InEnvelope, OutEnvelope } from '../src/lib/ipc/envelope';
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
  lastCmd(name: string): OutEnvelope {
    const e = [...this.sent].reverse().find((s) => s.kind === 'cmd' && s.name === name);
    if (!e) throw new Error(`no ${name} cmd`);
    return e;
  }
  reqCount(name: string): number {
    return this.sent.filter((s) => s.kind === 'req' && s.name === name).length;
  }
}

const track = (id: string) => ({ video_id: id, title: id }) as unknown as TrackModel;
const summary = (id: string, count = 0): PlaylistSummary => ({
  id,
  name: id,
  count,
  description: null,
});
const snapshot = (items: PlaylistSummary[]): PlaylistsSnapshot => ({
  kind: 'playlists_snapshot',
  items,
});
const detail = (id: string, tracks: TrackModel[]): PlaylistDetail => ({
  id,
  name: id,
  description: null,
  tracks,
});

describe('PlaylistsStore', () => {
  it('mirrors the playlists snapshot', () => {
    const t = new MockTransport();
    const store = new PlaylistsStore(new Client(t));
    t.emit({ v: 1, kind: 'event', topic: 'playlists', payload: snapshot([summary('a', 2)]) });
    expect(store.list.length).toBe(1);
    expect(store.list[0].count).toBe(2);
  });

  it('submitCreate sends playlist_create and ignores a blank name', () => {
    const t = new MockTransport();
    const store = new PlaylistsStore(new Client(t));
    store.beginCreate();
    store.submitCreate('  ');
    expect(t.sent.some((s) => s.kind === 'cmd' && s.name === 'playlist_create')).toBe(false);
    store.submitCreate('Mix');
    expect(t.lastCmd('playlist_create').payload).toMatchObject({ name: 'Mix' });
    expect(store.createOpen).toBe(false);
  });

  it('confirmDelete sends playlist_delete and closes an open matching detail', () => {
    const t = new MockTransport();
    const store = new PlaylistsStore(new Client(t));
    store.detail = detail('pl-1', [track('x')]);
    store.beginDelete(summary('pl-1', 1));
    store.confirmDelete();
    expect(t.lastCmd('playlist_delete').payload).toMatchObject({ playlist_id: 'pl-1' });
    expect(store.detail).toBeNull();
    expect(store.deleteTarget).toBeNull();
  });

  it('addTo sends playlist_add_tracks with the pending track and clears the picker', () => {
    const t = new MockTransport();
    const store = new PlaylistsStore(new Client(t));
    store.beginAdd(track('vid-9'));
    store.addTo('pl-2');
    expect(t.lastCmd('playlist_add_tracks').payload).toMatchObject({
      playlist_id: 'pl-2',
      video_ids: ['vid-9'],
    });
    expect(store.addTarget).toBeNull();
  });

  it('removeTrack and play send their commands', () => {
    const t = new MockTransport();
    const store = new PlaylistsStore(new Client(t));
    store.removeTrack('pl-1', 'vid-3');
    expect(t.lastCmd('playlist_remove_track').payload).toMatchObject({
      playlist_id: 'pl-1',
      video_id: 'vid-3',
    });
    store.play('pl-1');
    expect(t.lastCmd('playlist_play').payload).toMatchObject({ playlist_id: 'pl-1' });
  });

  it('open() fetches the detail; a stale fetch is dropped', async () => {
    const t = new MockTransport();
    const store = new PlaylistsStore(new Client(t));
    const first = store.open('pl-1');
    const firstReq = t.lastReq('fetch_playlist_detail');
    const second = store.open('pl-2');
    const secondReq = t.lastReq('fetch_playlist_detail');

    // The late reply to the superseded 'pl-1' open must not land over 'pl-2'.
    t.emit({ v: 1, id: firstReq.id, kind: 'res', payload: detail('pl-1', [track('a')]) });
    t.emit({ v: 1, id: secondReq.id, kind: 'res', payload: detail('pl-2', [track('b'), track('c')]) });
    await Promise.all([first, second]);

    expect(store.detail?.id).toBe('pl-2');
    expect(store.detail?.tracks.length).toBe(2);
    expect(store.detailLoading).toBe(false);
  });

  it('a snapshot re-fetches an open detail, or drops it when the playlist is gone', async () => {
    const t = new MockTransport();
    const store = new PlaylistsStore(new Client(t));
    const open = store.open('pl-1');
    t.emit({ v: 1, id: t.lastReq('fetch_playlist_detail').id, kind: 'res', payload: detail('pl-1', []) });
    await open;

    // Still present ⇒ a membership change re-pulls the detail.
    const before = t.reqCount('fetch_playlist_detail');
    t.emit({ v: 1, kind: 'event', topic: 'playlists', payload: snapshot([summary('pl-1', 1)]) });
    expect(t.reqCount('fetch_playlist_detail')).toBe(before + 1);

    // Vanished ⇒ the drill-down closes.
    t.emit({ v: 1, kind: 'event', topic: 'playlists', payload: snapshot([summary('pl-9')]) });
    expect(store.detail).toBeNull();
  });
});

describe('demo core playlists', () => {
  beforeEach(() => vi.useFakeTimers());
  afterEach(() => vi.useRealTimers());

  function boot() {
    const t = new DemoCoreTransport();
    const frames: InEnvelope[] = [];
    t.onMessage((e) => frames.push(e));
    vi.advanceTimersByTime(200);
    t.send({ v: 1, kind: 'sub', name: 'subscribe', payload: ['player', 'queue', 'playlists'] });
    vi.advanceTimersByTime(50);
    return { t, frames };
  }
  const lastList = (frames: InEnvelope[]) =>
    ([...frames].reverse().find((e) => e.kind === 'event' && e.topic === 'playlists')!
      .payload as PlaylistsSnapshot).items;
  const detailById = (frames: InEnvelope[], id: number) =>
    [...frames].reverse().find((e) => e.kind === 'res' && e.id === id)!.payload as PlaylistDetail;

  it('subscribe pushes the seeded playlist list', () => {
    const { frames } = boot();
    const items = lastList(frames);
    expect(items.length).toBe(2);
    expect(items.find((p) => p.id === 'pl-1')?.count).toBe(3);
  });

  it('fetch_playlist_detail returns the playlist tracks', () => {
    const { t, frames } = boot();
    t.send({ v: 1, id: 1, kind: 'req', name: 'fetch_playlist_detail', payload: { playlist_id: 'pl-1' } });
    vi.advanceTimersByTime(50);
    expect(detailById(frames, 1).tracks.length).toBe(3);
  });

  it('create then delete grows and shrinks the list', () => {
    const { t, frames } = boot();
    t.send({ v: 1, kind: 'cmd', name: 'playlist_create', payload: { name: 'Roadtrip' } });
    vi.advanceTimersByTime(50);
    const created = lastList(frames).find((p) => p.name === 'Roadtrip')!;
    expect(created).toBeTruthy();
    expect(created.count).toBe(0);

    t.send({ v: 1, kind: 'cmd', name: 'playlist_delete', payload: { playlist_id: created.id } });
    vi.advanceTimersByTime(50);
    expect(lastList(frames).some((p) => p.id === created.id)).toBe(false);
  });

  it('add then remove a track reconciles the count', () => {
    const { t, frames } = boot();
    t.send({
      v: 1,
      kind: 'cmd',
      name: 'playlist_add_tracks',
      payload: { playlist_id: 'pl-1', video_ids: ['demo-001'] },
    });
    vi.advanceTimersByTime(50);
    expect(lastList(frames).find((p) => p.id === 'pl-1')?.count).toBe(4);

    t.send({
      v: 1,
      kind: 'cmd',
      name: 'playlist_remove_track',
      payload: { playlist_id: 'pl-1', video_id: 'demo-001' },
    });
    vi.advanceTimersByTime(50);
    expect(lastList(frames).find((p) => p.id === 'pl-1')?.count).toBe(3);
  });

  it('playlist_play replaces the queue with the playlist', () => {
    const { t, frames } = boot();
    t.send({ v: 1, kind: 'cmd', name: 'playlist_play', payload: { playlist_id: 'pl-1' } });
    vi.advanceTimersByTime(50);
    const q = [...frames].reverse().find((e) => e.kind === 'event' && e.topic === 'queue')!
      .payload as { model: { items: TrackModel[] } };
    expect(q.model.items.length).toBe(3);
  });
});
