// DJ Gem chat wiring (docs/gui/07 §12): the store's optimistic user bubble + push replace,
// and the demo core's ask_ai thinking→reply→suggestions flow (suggestions play via play_tracks).

import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { Client } from '../src/lib/ipc/client';
import { AiStore, type AiState } from '../src/lib/stores/ai.svelte';
import { DemoCoreTransport } from '../src/lib/dev/democore';
import type { Transport } from '../src/lib/ipc/transport';
import type { InEnvelope, OutEnvelope } from '../src/lib/ipc/envelope';
import type { PlayerModel } from '../src/generated/protocol/PlayerModel';
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

describe('AiStore', () => {
  it('ask adds an optimistic user bubble, marks thinking, and sends ask_ai', () => {
    const t = new MockTransport();
    const store = new AiStore(new Client(t));
    store.ask('play something jazzy');
    expect(store.messages.at(-1)).toEqual({ role: 'user', text: 'play something jazzy' });
    expect(store.thinking).toBe(true);
    expect(store.started).toBe(true);
    const sent = t.sent.at(-1)!;
    expect(sent).toMatchObject({ kind: 'cmd', name: 'ask_ai' });
    expect((sent.payload as { prompt: string }).prompt).toBe('play something jazzy');
  });

  it('an ai_state push replaces transcript, thinking, and suggestions', () => {
    const t = new MockTransport();
    const store = new AiStore(new Client(t));
    store.ask('hi');
    const state: AiState = {
      kind: 'ai_state',
      messages: [
        { role: 'user', text: 'hi' },
        { role: 'assistant', text: 'yo' },
      ],
      thinking: false,
      suggestions: [stub('a')],
    };
    t.emit({ v: 1, kind: 'event', topic: 'ai', payload: state });
    expect(store.messages.length).toBe(2);
    expect(store.thinking).toBe(false);
    expect(store.suggestions.length).toBe(1);
  });

  it('ignores a blank prompt', () => {
    const t = new MockTransport();
    const store = new AiStore(new Client(t));
    store.ask('   ');
    expect(store.messages.length).toBe(0);
    expect(t.sent.length).toBe(0);
  });
});

describe('demo core DJ Gem', () => {
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
  const lastAi = (frames: InEnvelope[]) =>
    [...frames].reverse().find((e) => e.kind === 'event' && e.topic === 'ai')!.payload as AiState;
  const lastPlayer = (frames: InEnvelope[]) =>
    ([...frames].reverse().find((e) => e.kind === 'event' && e.topic === 'player')!.payload as {
      model: PlayerModel;
    }).model;

  it('ask_ai pushes thinking first, then a reply with suggestions', () => {
    const { t, frames } = boot();
    t.send({ v: 1, kind: 'cmd', name: 'ask_ai', payload: { ticket: 1, prompt: 'purrple' } });
    vi.advanceTimersByTime(20);
    expect(lastAi(frames).thinking).toBe(true);
    expect(lastAi(frames).messages.at(-1)!.role).toBe('user');

    vi.advanceTimersByTime(500);
    const done = lastAi(frames);
    expect(done.thinking).toBe(false);
    expect(done.messages.at(-1)!.role).toBe('assistant');
    expect(done.suggestions.length).toBeGreaterThan(0);
  });

  it('a suggested track plays via play_tracks', () => {
    const { t, frames } = boot();
    t.send({ v: 1, kind: 'cmd', name: 'ask_ai', payload: { ticket: 1, prompt: 'tailwind' } });
    vi.advanceTimersByTime(500);
    const pick = lastAi(frames).suggestions[0];
    t.send({ v: 1, kind: 'cmd', name: 'play_tracks', payload: { video_ids: [pick.video_id] } });
    vi.advanceTimersByTime(50);
    expect(lastPlayer(frames).track?.video_id).toBe(pick.video_id);
  });
});
