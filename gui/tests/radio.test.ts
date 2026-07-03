// Radio mode (docs/gui/07 §16): the rail switch flips player.radio_mode via the RadioMode
// setting change; the store sends it and the demo core reflects it on the next player push.

import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { Client } from '../src/lib/ipc/client';
import { PlaybackStore } from '../src/lib/stores/playback.svelte';
import { DemoCoreTransport } from '../src/lib/dev/democore';
import type { Transport } from '../src/lib/ipc/transport';
import type { InEnvelope, OutEnvelope } from '../src/lib/ipc/envelope';
import type { PlayerModel } from '../src/generated/protocol/PlayerModel';

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

describe('PlaybackStore.setRadioMode', () => {
  it('sends the radio_mode setting change with on/off', () => {
    const t = new MockTransport();
    const store = new PlaybackStore(new Client(t));
    store.setRadioMode(true);
    expect(t.sent.at(-1)).toMatchObject({
      kind: 'cmd',
      name: 'set_setting',
      payload: { change: { setting: 'radio_mode', state: 'on' } },
    });
    store.setRadioMode(false);
    expect((t.sent.at(-1)!.payload as { change: { state: string } }).change.state).toBe('off');
  });
});

describe('demo core radio mode', () => {
  beforeEach(() => vi.useFakeTimers());
  afterEach(() => vi.useRealTimers());

  function boot() {
    const t = new DemoCoreTransport();
    const frames: InEnvelope[] = [];
    t.onMessage((e) => frames.push(e));
    vi.advanceTimersByTime(200);
    t.send({ v: 1, kind: 'sub', name: 'subscribe', payload: ['player'] });
    vi.advanceTimersByTime(50);
    return { t, frames };
  }
  const lastPlayer = (frames: InEnvelope[]) =>
    ([...frames].reverse().find((e) => e.kind === 'event' && e.topic === 'player')!.payload as {
      model: PlayerModel;
    }).model;

  it('set_setting radio_mode flips player.radio_mode', () => {
    const { t, frames } = boot();
    expect(lastPlayer(frames).radio_mode).toBe(false);

    t.send({
      v: 1,
      kind: 'cmd',
      name: 'set_setting',
      payload: { change: { setting: 'radio_mode', state: 'on' } },
    });
    vi.advanceTimersByTime(50);
    expect(lastPlayer(frames).radio_mode).toBe(true);

    t.send({
      v: 1,
      kind: 'cmd',
      name: 'set_setting',
      payload: { change: { setting: 'radio_mode', state: 'off' } },
    });
    vi.advanceTimersByTime(50);
    expect(lastPlayer(frames).radio_mode).toBe(false);
  });
});
