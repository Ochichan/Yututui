// Transfer wizard wiring (docs/gui/07 §14): the store sends the three commands and mirrors
// the `transfer` topic state machine; the demo core walks idle → listing → ready → running →
// done with a coalesced-progress ramp and a match report.

import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { Client } from '../src/lib/ipc/client';
import { TransferStore, type TransferState } from '../src/lib/stores/transfer.svelte';
import { DemoCoreTransport } from '../src/lib/dev/democore';
import type { Transport } from '../src/lib/ipc/transport';
import type { InEnvelope, OutEnvelope } from '../src/lib/ipc/envelope';

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
  has(name: string): boolean {
    return this.sent.some((s) => s.kind === 'cmd' && s.name === name);
  }
  lastCmd(name: string): OutEnvelope {
    const e = [...this.sent].reverse().find((s) => s.kind === 'cmd' && s.name === name);
    if (!e) throw new Error(`no ${name} cmd`);
    return e;
  }
}

describe('TransferStore', () => {
  it('listSpotify / cancel send their commands', () => {
    const t = new MockTransport();
    const store = new TransferStore(new Client(t));
    store.listSpotify();
    store.cancel();
    expect(t.has('transfer_list_spotify')).toBe(true);
    expect(t.has('transfer_cancel')).toBe(true);
  });

  it('start sends transfer_start with the spec, but no-ops on an empty selection', () => {
    const t = new MockTransport();
    const store = new TransferStore(new Client(t));
    store.start({ source_ids: [], dest: { kind: 'new', name: 'x' } });
    expect(t.has('transfer_start')).toBe(false);
    store.start({ source_ids: ['sp-1'], dest: { kind: 'existing', playlist_id: 'pl-1' } });
    expect(t.lastCmd('transfer_start').payload).toMatchObject({
      spec: { source_ids: ['sp-1'], dest: { kind: 'existing', playlist_id: 'pl-1' } },
    });
  });

  it('mirrors the topic state and reset() returns to idle', () => {
    const t = new MockTransport();
    const store = new TransferStore(new Client(t));
    const running: TransferState = {
      kind: 'transfer_state',
      phase: 'running',
      sources: [{ id: 'sp-1', name: 'A', count: 10 }],
      job: { done: 5, total: 10, matched: 5, failed: 0 },
      report: null,
      error: null,
    };
    t.emit({ v: 1, kind: 'event', topic: 'transfer', payload: running });
    expect(store.phase).toBe('running');
    store.reset();
    expect(store.phase).toBe('idle');
    expect(store.state.sources.length).toBe(0);
  });
});

describe('demo core transfer', () => {
  beforeEach(() => vi.useFakeTimers());
  afterEach(() => vi.useRealTimers());

  function boot() {
    const t = new DemoCoreTransport();
    const frames: InEnvelope[] = [];
    t.onMessage((e) => frames.push(e));
    vi.advanceTimersByTime(200);
    return { t, frames };
  }
  const last = (frames: InEnvelope[]) =>
    [...frames].reverse().find((e) => e.kind === 'event' && e.topic === 'transfer')!
      .payload as TransferState;

  it('list walks listing → ready with sources', () => {
    const { t, frames } = boot();
    t.send({ v: 1, kind: 'cmd', name: 'transfer_list_spotify', payload: {} });
    vi.advanceTimersByTime(20);
    expect(last(frames).phase).toBe('listing');
    vi.advanceTimersByTime(250);
    const ready = last(frames);
    expect(ready.phase).toBe('ready');
    expect(ready.sources.length).toBeGreaterThan(0);
  });

  it('start runs to done with a report over the selected totals', () => {
    const { t, frames } = boot();
    t.send({ v: 1, kind: 'cmd', name: 'transfer_list_spotify', payload: {} });
    vi.advanceTimersByTime(250);
    const sources = last(frames).sources;
    const pick = sources[0];

    t.send({
      v: 1,
      kind: 'cmd',
      name: 'transfer_start',
      payload: { spec: { source_ids: [pick.id], dest: { kind: 'new', name: 'Imported' } } },
    });
    vi.advanceTimersByTime(20);
    expect(last(frames).phase).toBe('running');

    vi.advanceTimersByTime(1000);
    const done = last(frames);
    expect(done.phase).toBe('done');
    expect(done.job?.total).toBe(pick.count);
    expect(done.report?.matched).toBe(pick.count - (done.report?.failed ?? 0));
    expect(done.report?.dest).toContain('Imported');
  });

  it('cancel mid-run returns to idle', () => {
    const { t, frames } = boot();
    t.send({ v: 1, kind: 'cmd', name: 'transfer_list_spotify', payload: {} });
    vi.advanceTimersByTime(250);
    const pick = last(frames).sources[0];
    t.send({
      v: 1,
      kind: 'cmd',
      name: 'transfer_start',
      payload: { spec: { source_ids: [pick.id], dest: { kind: 'new', name: 'X' } } },
    });
    vi.advanceTimersByTime(160);
    t.send({ v: 1, kind: 'cmd', name: 'transfer_cancel', payload: {} });
    vi.advanceTimersByTime(50);
    expect(last(frames).phase).toBe('idle');
    // A late progress tick after cancel must not resurrect the run.
    vi.advanceTimersByTime(1000);
    expect(last(frames).phase).toBe('idle');
  });
});
