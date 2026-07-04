// The animation runtime (docs/gui/06 §5, stores/anim.svelte.ts). The behavioural contract:
// master off (or reduced-motion) ⇒ `<html class="no-anim">` and a cancelled rAF loop; the
// shared ticker self-suspends to zero callbacks when nothing is subscribed, when unfocused
// under pause_unfocused, or when disabled; and it throttles delivery to the fps target.
// rAF and matchMedia are stubbed so the loop is driven deterministically, no real timers.

import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { Client } from '../src/lib/ipc/client';
import {
  AnimStore,
  clampFps,
  defaultAnimations,
  EFFECT_IDS,
  FPS_DEFAULT,
  FPS_MAX,
  FPS_MIN,
  type AnimationsModel,
} from '../src/lib/stores/anim.svelte';
import type { InEnvelope, OutEnvelope } from '../src/lib/ipc/envelope';
import type { Transport } from '../src/lib/ipc/transport';

class MockTransport implements Transport {
  readonly live = false;
  #cb: ((env: InEnvelope) => void) | null = null;
  send(_env: OutEnvelope): void {}
  onMessage(cb: (env: InEnvelope) => void): void {
    this.#cb = cb;
  }
  emit(env: InEnvelope): void {
    this.#cb?.(env);
  }
}

// A single-slot rAF stub (the store keeps at most one frame in flight). `tick(now)` invokes
// the pending callback with a timestamp; the callback re-arms itself, mirroring real rAF.
let scheduled: { id: number; cb: FrameRequestCallback } | null = null;
let nextRafId = 1;

function installRaf(): void {
  scheduled = null;
  nextRafId = 1;
  vi.stubGlobal('requestAnimationFrame', (cb: FrameRequestCallback) => {
    const id = nextRafId++;
    scheduled = { id, cb };
    return id;
  });
  vi.stubGlobal('cancelAnimationFrame', (id: number) => {
    if (scheduled?.id === id) scheduled = null;
  });
}

function tick(now: number): void {
  const s = scheduled;
  if (!s) return;
  scheduled = null;
  s.cb(now);
}

function setReducedMotion(matches: boolean): void {
  vi.stubGlobal('matchMedia', (query: string) => ({
    matches,
    media: query,
    onchange: null,
    addEventListener() {},
    removeEventListener() {},
    addListener() {},
    removeListener() {},
    dispatchEvent() {
      return false;
    },
  }));
}

function push(t: MockTransport, animations: AnimationsModel): void {
  // The store only reads model.animations; payload is `unknown` on the wire.
  t.emit({
    v: 1,
    kind: 'event',
    topic: 'settings',
    payload: { kind: 'settings_snapshot', model: { animations } },
  } as InEnvelope);
}

function make(animations: AnimationsModel = defaultAnimations()) {
  const t = new MockTransport();
  const store = new AnimStore(new Client(t));
  push(t, animations);
  return { t, store };
}

const on = (over: Partial<AnimationsModel>): AnimationsModel => ({
  ...defaultAnimations(),
  master: true,
  ...over,
});

const hasNoAnim = () => document.documentElement.classList.contains('no-anim');

describe('AnimStore', () => {
  beforeEach(() => {
    installRaf();
    setReducedMotion(false);
    document.documentElement.classList.remove('no-anim');
  });
  afterEach(() => {
    vi.unstubAllGlobals();
    document.documentElement.classList.remove('no-anim');
  });

  it('defaults: master off ⇒ no-anim class, disabled, ticker idle', () => {
    const { store } = make();
    expect(store.enabled).toBe(false);
    expect(hasNoAnim()).toBe(true);
    expect(store.running).toBe(false);
  });

  it('master on lifts no-anim but the loop stays idle until a subscriber joins', () => {
    const { store } = make(on({}));
    expect(store.enabled).toBe(true);
    expect(hasNoAnim()).toBe(false);
    expect(store.running).toBe(false); // ambient contract: no work ⇒ no loop

    const stop = store.frame(() => {});
    expect(store.running).toBe(true);
    stop();
    expect(store.running).toBe(false); // last subscriber left ⇒ loop collapses
  });

  it('delivers frames to subscribers, throttled to the fps target', () => {
    const { store } = make(on({ fps: 30 })); // 30 fps ⇒ ~33 ms between frames
    const hits: number[] = [];
    store.frame((now) => hits.push(now));
    expect(store.running).toBe(true);

    tick(100); // first frame always delivers
    tick(110); // +10 ms < 33 ms ⇒ throttled
    tick(140); // +40 ms since last delivered ⇒ delivers

    expect(hits).toEqual([100, 140]);
    expect(store.frameCount).toBe(2);
  });

  it('turning master off cancels the loop and re-adds no-anim', () => {
    const { t, store } = make(on({}));
    store.frame(() => {});
    expect(store.running).toBe(true);

    push(t, defaultAnimations()); // master back to false
    expect(store.running).toBe(false);
    expect(hasNoAnim()).toBe(true);
  });

  it('pause_unfocused parks the ticker on blur and resumes on focus', () => {
    const { store } = make(on({ pause_unfocused: true }));
    store.frame(() => {});
    expect(store.running).toBe(true);

    window.dispatchEvent(new Event('blur'));
    expect(store.running).toBe(false);

    window.dispatchEvent(new Event('focus'));
    expect(store.running).toBe(true);
  });

  it('prefers-reduced-motion forces master off regardless of config', () => {
    setReducedMotion(true); // read in the constructor
    const { store } = make(on({}));
    expect(store.enabled).toBe(false);
    expect(hasNoAnim()).toBe(true);

    const stop = store.frame(() => {});
    expect(store.running).toBe(false);
    stop();
  });

  it('isOn gates each effect on the effective master', () => {
    const { t, store } = make(on({ heart: true }));
    expect(store.isOn('heart')).toBe(true);
    expect(store.isOn('title')).toBe(false);

    push(t, on({ master: false, heart: true }));
    expect(store.isOn('heart')).toBe(false); // master off ⇒ nothing renders
  });
});

describe('animation helpers', () => {
  it('clampFps clamps to [FPS_MIN, FPS_MAX] and falls back on NaN', () => {
    expect(clampFps(3)).toBe(FPS_MIN);
    expect(clampFps(200)).toBe(FPS_MAX);
    expect(clampFps(24)).toBe(24);
    expect(clampFps(30.6)).toBe(31);
    expect(clampFps(Number.NaN)).toBe(FPS_DEFAULT);
  });

  it('defaultAnimations mirrors the core defaults (all effects off, 30 fps)', () => {
    const d = defaultAnimations();
    expect(d.master).toBe(false);
    expect(d.pause_unfocused).toBe(true);
    expect(d.fps).toBe(FPS_DEFAULT);
    expect(EFFECT_IDS).toHaveLength(25);
    for (const id of EFFECT_IDS) expect(d[id]).toBe(false);
    // 25 effects + master + pause_unfocused + fps.
    expect(Object.keys(d)).toHaveLength(28);
  });
});
