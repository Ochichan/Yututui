// Why-DJ-Gem popover wiring (docs/gui/07 §13): the store's provenance set + seq-guarded
// fetch-on-open, and the demo core's provenance push / fetch_why_gem reply / mark-on-enqueue.

import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { Client } from '../src/lib/ipc/client';
import { WhyGemStore, type WhyGem } from '../src/lib/stores/whygem.svelte';
import { DemoCoreTransport } from '../src/lib/dev/democore';
import type { Transport } from '../src/lib/ipc/transport';
import type { InEnvelope, OutEnvelope } from '../src/lib/ipc/envelope';
import type { AiState } from '../src/lib/stores/ai.svelte';

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
  reply(id: number, payload: unknown): void {
    this.#cb?.({ v: 1, id, kind: 'res', payload });
  }
}

const gem = (slot: string): WhyGem => ({ slot, reasons: ['a', 'b'], confidence: 0.5 });

describe('WhyGemStore', () => {
  it('tracks provenance from the ai topic and only reacts to its own kind', () => {
    const t = new MockTransport();
    const store = new WhyGemStore(new Client(t));
    // An ai_state push (the chat store's kind) must not touch provenance.
    const chat: AiState = { kind: 'ai_state', messages: [], thinking: false, suggestions: [] };
    t.emit({ v: 1, kind: 'event', topic: 'ai', payload: chat });
    expect(store.has('x')).toBe(false);

    t.emit({
      v: 1,
      kind: 'event',
      topic: 'ai',
      payload: { kind: 'why_gem_provenance', video_ids: ['x', 'y'] },
    });
    expect(store.has('x')).toBe(true);
    expect(store.has('z')).toBe(false);
  });

  it('open fetches the explanation and stores the anchor', async () => {
    const t = new MockTransport();
    const store = new WhyGemStore(new Client(t));
    const p = store.open('x', { x: 100, y: 40 });
    expect(store.openId).toBe('x');
    expect(store.anchor).toEqual({ x: 100, y: 40 });
    expect(store.loading).toBe(true);
    const req = t.sent.at(-1)!;
    expect(req).toMatchObject({ kind: 'req', name: 'fetch_why_gem' });
    expect((req.payload as { video_id: string }).video_id).toBe('x');

    t.reply(req.id!, gem('Deep cut'));
    await p;
    expect(store.loading).toBe(false);
    expect(store.detail?.slot).toBe('Deep cut');
  });

  it('a stale fetch cannot paint after close', async () => {
    const t = new MockTransport();
    const store = new WhyGemStore(new Client(t));
    const p = store.open('x', { x: 0, y: 0 });
    const req = t.sent.at(-1)!;
    store.close();
    t.reply(req.id!, gem('late'));
    await p;
    expect(store.openId).toBeNull();
    expect(store.detail).toBeNull();
  });

  it('closes when the open row loses gem status on a provenance push', () => {
    const t = new MockTransport();
    const store = new WhyGemStore(new Client(t));
    t.emit({
      v: 1,
      kind: 'event',
      topic: 'ai',
      payload: { kind: 'why_gem_provenance', video_ids: ['x'] },
    });
    void store.open('x', { x: 0, y: 0 });
    t.emit({
      v: 1,
      kind: 'event',
      topic: 'ai',
      payload: { kind: 'why_gem_provenance', video_ids: [] },
    });
    expect(store.openId).toBeNull();
  });
});

describe('demo core why-gem', () => {
  beforeEach(() => vi.useFakeTimers());
  afterEach(() => vi.useRealTimers());

  function boot() {
    const t = new DemoCoreTransport();
    const frames: InEnvelope[] = [];
    t.onMessage((e) => frames.push(e));
    vi.advanceTimersByTime(200);
    t.send({ v: 1, kind: 'sub', name: 'subscribe', payload: ['player', 'queue', 'ai'] });
    vi.advanceTimersByTime(50);
    return { t, frames };
  }
  const lastProvenance = (frames: InEnvelope[]) =>
    [...frames]
      .reverse()
      .map((e) => (e.kind === 'event' && e.topic === 'ai' ? e.payload : null))
      .find((p): p is { kind: string; video_ids: string[] } =>
        Boolean(p && (p as { kind?: string }).kind === 'why_gem_provenance'),
      )!;

  it('pushes seeded provenance on ai subscribe', () => {
    const { frames } = boot();
    const prov = lastProvenance(frames);
    expect(prov.video_ids).toContain('demo-006');
    expect(prov.video_ids).toContain('demo-008');
  });

  it('answers fetch_why_gem for a seeded pick and null for a plain row', () => {
    const { t, frames } = boot();
    t.send({ v: 1, id: 90, kind: 'req', name: 'fetch_why_gem', payload: { video_id: 'demo-006' } });
    t.send({ v: 1, id: 91, kind: 'req', name: 'fetch_why_gem', payload: { video_id: 'demo-001' } });
    vi.advanceTimersByTime(50);
    const res = (id: number) =>
      [...frames].reverse().find((e) => e.kind === 'res' && e.id === id)!.payload;
    expect((res(90) as WhyGem).slot).toBe('More like this');
    expect(res(91)).toBeNull();
  });

  it('marks a DJ-Gem suggestion as a pick once it is enqueued', () => {
    const { t, frames } = boot();
    t.send({ v: 1, kind: 'cmd', name: 'ask_ai', payload: { ticket: 1, prompt: 'meownlight' } });
    vi.advanceTimersByTime(500);
    const pick = (
      [...frames].reverse().find((e) => e.kind === 'event' && e.topic === 'ai')!.payload as AiState
    ).suggestions[0];
    expect(pick).toBeTruthy();

    t.send({ v: 1, kind: 'cmd', name: 'enqueue_tracks', payload: { video_ids: [pick.video_id] } });
    vi.advanceTimersByTime(50);
    expect(lastProvenance(frames).video_ids).toContain(pick.video_id);
  });
});
