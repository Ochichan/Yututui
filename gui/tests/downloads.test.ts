// Downloads wiring (docs/gui/07 §15): the store mirrors the downloads snapshot, and the demo
// core drives a normal download to done, fails a live stream, and honors delete_download.

import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { Client } from '../src/lib/ipc/client';
import { DownloadsStore, type DownloadsSnapshot } from '../src/lib/stores/downloads.svelte';
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
}

const track = (id: string) => ({ video_id: id, title: id }) as unknown as TrackModel;

describe('DownloadsStore', () => {
  it('download sends the command; a snapshot updates items and active count', () => {
    const t = new MockTransport();
    const store = new DownloadsStore(new Client(t));
    store.download(track('a'));
    const sent = t.sent.at(-1)!;
    expect(sent).toMatchObject({ kind: 'cmd', name: 'download' });
    expect((sent.payload as { video_id: string }).video_id).toBe('a');

    const snap: DownloadsSnapshot = {
      kind: 'downloads_snapshot',
      items: [
        { video_id: 'a', title: 'a', state: 'running', pct: 40, error: null },
        { video_id: 'b', title: 'b', state: 'done', pct: 100, error: null },
      ],
    };
    t.emit({ v: 1, kind: 'event', topic: 'downloads', payload: snap });
    expect(store.items.length).toBe(2);
    expect(store.active).toBe(1);
  });

  it('remove sends delete_download with the delete_file flag', () => {
    const t = new MockTransport();
    const store = new DownloadsStore(new Client(t));
    store.remove({ video_id: 'x', title: 'x', state: 'done', pct: 100, error: null }, true);
    expect(t.sent.at(-1)).toMatchObject({
      kind: 'cmd',
      name: 'delete_download',
      payload: { video_id: 'x', delete_file: true },
    });
  });
});

describe('demo core downloads', () => {
  beforeEach(() => vi.useFakeTimers());
  afterEach(() => vi.useRealTimers());

  function boot() {
    const t = new DemoCoreTransport();
    const frames: InEnvelope[] = [];
    t.onMessage((e) => frames.push(e));
    vi.advanceTimersByTime(200);
    t.send({ v: 1, kind: 'sub', name: 'subscribe', payload: ['player', 'queue', 'downloads'] });
    vi.advanceTimersByTime(50);
    return { t, frames };
  }
  const lastDl = (frames: InEnvelope[]) =>
    [...frames].reverse().find((e) => e.kind === 'event' && e.topic === 'downloads')!
      .payload as DownloadsSnapshot;
  const item = (frames: InEnvelope[], id: string) =>
    lastDl(frames).items.find((d) => d.video_id === id);

  it('a normal track progresses running → done', () => {
    const { t, frames } = boot();
    t.send({ v: 1, kind: 'cmd', name: 'download', payload: { video_id: 'demo-006', title: 'Tailwind' } });
    vi.advanceTimersByTime(20);
    expect(item(frames, 'demo-006')!.state).toBe('running');
    vi.advanceTimersByTime(1000);
    expect(item(frames, 'demo-006')!.state).toBe('done');
    expect(item(frames, 'demo-006')!.pct).toBe(100);
  });

  it('a live stream fails with a reason', () => {
    const { t, frames } = boot();
    t.send({ v: 1, kind: 'cmd', name: 'download', payload: { video_id: 'demo-009', title: 'ON AIR' } });
    vi.advanceTimersByTime(400);
    const d = item(frames, 'demo-009')!;
    expect(d.state).toBe('failed');
    expect(d.error).toBeTruthy();
  });

  it('delete_download removes the entry', () => {
    const { t, frames } = boot();
    t.send({ v: 1, kind: 'cmd', name: 'download', payload: { video_id: 'demo-006', title: 'Tailwind' } });
    vi.advanceTimersByTime(1000);
    t.send({ v: 1, kind: 'cmd', name: 'delete_download', payload: { video_id: 'demo-006' } });
    vi.advanceTimersByTime(50);
    expect(item(frames, 'demo-006')).toBeUndefined();
  });
});
