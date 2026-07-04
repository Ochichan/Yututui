// Settings wiring (docs/gui/05 §5.2, 07 §6–§10): the store's pending-overlay merge rule is
// the keystone — an optimistic edit survives until the authoritative push confirms it, and a
// stale round-trip must never revert an in-flight edit. The demo core answers `apply`,
// `set_gemini_key`, `clear_romanization_cache`, and `reset_all_settings`.

import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { Client } from '../src/lib/ipc/client';
import { SettingsStore, type SettingsModelV8 } from '../src/lib/stores/settings.svelte';
import { defaultAnimations } from '../src/lib/stores/anim.svelte';
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
}

function baseModel(): SettingsModelV8 {
  return {
    rev: 1,
    playback: {
      speed_tenths: 10,
      seek_seconds: 5,
      gapless: true,
      enqueue_next: false,
      autoplay_on_start: false,
      mouse_wheel_volume: true,
      media_controls: true,
      volume: 72,
      shuffle: false,
      repeat: 'off',
    },
    eq: { preset: 'flat', bands: [0, 0, 0, 0, 0, 0, 0, 0, 0, 0], normalize: false },
    streaming: {
      ai_enabled: false,
      gemini_model: 'gemini-2.5-flash',
      autoplay: false,
      mode: 'balanced',
      has_gemini_key: false,
    },
    search: {
      default_source: 'youtube',
      soundcloud_enabled: true,
      audius_enabled: true,
      jamendo_enabled: false,
      internet_archive_enabled: true,
      radio_browser_enabled: true,
      audius_app_name: 'ytm-tui',
      jamendo_client_id: null,
    },
    ui: { language: 'en', mouse: true, album_art: true, romanized_titles: false },
    storage: { download_dir: '~/Music/ytm-tui', cookies_file: null, download_concurrency: 3 },
    animations: defaultAnimations(),
  };
}

function push(t: MockTransport, model: SettingsModelV8): void {
  t.emit({ v: 1, kind: 'event', topic: 'settings', payload: { kind: 'settings_snapshot', model } });
}

describe('SettingsStore', () => {
  it('starts empty and mirrors the first snapshot', () => {
    const t = new MockTransport();
    const store = new SettingsStore(new Client(t));
    expect(store.model).toBeNull();
    expect(store.playback).toBeNull();
    push(t, baseModel());
    expect(store.model).not.toBeNull();
    expect(store.playback?.gapless).toBe(true);
  });

  it('apply sends the grouped change and overlays optimistically before any push', () => {
    const t = new MockTransport();
    const store = new SettingsStore(new Client(t));
    push(t, baseModel());

    store.apply('playback', 'gapless', false);
    expect(t.sent.at(-1)).toMatchObject({
      kind: 'cmd',
      name: 'apply',
      payload: { change: { group: 'playback', field: 'gapless', value: false } },
    });
    // Optimistic: the merged view shows the pending value while the model still says true.
    expect(store.playback?.gapless).toBe(false);
    expect(store.model?.playback.gapless).toBe(true);
    expect(store.dirty).toBe(true);
  });

  it('clears a pending edit once a push confirms it', () => {
    const t = new MockTransport();
    const store = new SettingsStore(new Client(t));
    push(t, baseModel());
    store.apply('playback', 'gapless', false);
    expect(store.dirty).toBe(true);

    const confirmed = baseModel();
    confirmed.playback.gapless = false; // the core agreed
    push(t, confirmed);

    expect(store.dirty).toBe(false);
    expect(store.playback?.gapless).toBe(false);
  });

  it('keeps an in-flight edit a stale push disagrees with (no revert)', () => {
    const t = new MockTransport();
    const store = new SettingsStore(new Client(t));
    push(t, baseModel());
    store.apply('playback', 'gapless', false);

    // A slow round-trip re-pushes the OLD value; the pending edit must survive.
    push(t, baseModel());

    expect(store.dirty).toBe(true);
    expect(store.playback?.gapless).toBe(false); // pending still wins
    expect(store.model?.playback.gapless).toBe(true);
  });

  it('uses value (not reference) equality for the 10-band EQ array', () => {
    const t = new MockTransport();
    const store = new SettingsStore(new Client(t));
    push(t, baseModel());
    const bands = [3, 0, 0, 0, 0, 0, 0, 0, 0, 0];
    store.apply('eq', 'bands', bands);
    expect(store.dirty).toBe(true);

    const confirmed = baseModel();
    confirmed.eq.bands = [3, 0, 0, 0, 0, 0, 0, 0, 0, 0]; // equal by value, different array
    push(t, confirmed);
    expect(store.dirty).toBe(false);
    expect(store.eq?.bands[0]).toBe(3);
  });

  it('two independent edits clear independently', () => {
    const t = new MockTransport();
    const store = new SettingsStore(new Client(t));
    push(t, baseModel());
    store.apply('ui', 'album_art', false);
    store.apply('streaming', 'mode', 'discovery');
    expect(store.dirty).toBe(true);

    const half = baseModel();
    half.ui.album_art = false; // only the first is confirmed
    push(t, half);

    expect(store.dirty).toBe(true); // mode edit still pending
    expect(store.ui?.album_art).toBe(false);
    expect(store.streaming?.mode).toBe('discovery');
  });

  it('exposes the animations block and overlays edits optimistically', () => {
    const t = new MockTransport();
    const store = new SettingsStore(new Client(t));
    push(t, baseModel());
    expect(store.animations?.master).toBe(false); // core default
    expect(store.animations?.fps).toBe(30);

    store.apply('animations', 'master', true);
    expect(t.sent.at(-1)).toMatchObject({
      kind: 'cmd',
      name: 'apply',
      payload: { change: { group: 'animations', field: 'master', value: true } },
    });
    expect(store.animations?.master).toBe(true); // pending wins
    expect(store.model?.animations.master).toBe(false); // authoritative unchanged
    expect(store.dirty).toBe(true);
  });

  it('setGeminiKey is write-only; resetAll drops the overlay', () => {
    const t = new MockTransport();
    const store = new SettingsStore(new Client(t));
    push(t, baseModel());

    store.setGeminiKey('AIzaSECRET');
    expect(t.sent.at(-1)).toMatchObject({
      kind: 'cmd',
      name: 'set_gemini_key',
      payload: { key: 'AIzaSECRET' },
    });

    store.apply('ui', 'mouse', false);
    expect(store.dirty).toBe(true);
    store.resetAll();
    expect(store.dirty).toBe(false);
    expect(t.sent.at(-1)).toMatchObject({ kind: 'cmd', name: 'reset_all_settings' });
  });
});

describe('demo core settings', () => {
  beforeEach(() => vi.useFakeTimers());
  afterEach(() => vi.useRealTimers());

  function boot() {
    const t = new DemoCoreTransport();
    const frames: InEnvelope[] = [];
    t.onMessage((e) => frames.push(e));
    vi.advanceTimersByTime(200);
    t.send({ v: 1, kind: 'sub', name: 'subscribe', payload: ['settings'] });
    vi.advanceTimersByTime(50);
    return { t, frames };
  }
  const lastSettings = (frames: InEnvelope[]): SettingsModelV8 =>
    (
      [...frames].reverse().find((e) => e.kind === 'event' && e.topic === 'settings')!
        .payload as { model: SettingsModelV8 }
    ).model;

  it('subscribing yields an initial settings snapshot', () => {
    const { frames } = boot();
    expect(lastSettings(frames).playback.gapless).toBe(true);
  });

  it('apply mutates the model and re-pushes', () => {
    const { t, frames } = boot();
    t.send({
      v: 1,
      kind: 'cmd',
      name: 'apply',
      payload: { change: { group: 'playback', field: 'gapless', value: false } },
    });
    vi.advanceTimersByTime(20);
    expect(lastSettings(frames).playback.gapless).toBe(false);
  });

  it('an EQ preset change recomputes the bands', () => {
    const { t, frames } = boot();
    t.send({
      v: 1,
      kind: 'cmd',
      name: 'apply',
      payload: { change: { group: 'eq', field: 'preset', value: 'bass' } },
    });
    vi.advanceTimersByTime(20);
    const eq = lastSettings(frames).eq;
    expect(eq.preset).toBe('bass');
    expect(eq.bands[0]).toBeGreaterThan(0); // bass lifts the low bands
  });

  it('a manual band edit flips the preset to custom', () => {
    const { t, frames } = boot();
    t.send({
      v: 1,
      kind: 'cmd',
      name: 'apply',
      payload: { change: { group: 'eq', field: 'bands', value: [0, 0, 0, 0, 0, 0, 0, 0, 0, 0] } },
    });
    vi.advanceTimersByTime(20);
    expect(lastSettings(frames).eq.preset).toBe('custom');
  });

  it('set_gemini_key flips presence without echoing the key', () => {
    const { t, frames } = boot();
    t.send({ v: 1, kind: 'cmd', name: 'set_gemini_key', payload: { key: 'AIzaSECRET' } });
    vi.advanceTimersByTime(20);
    const snap = lastSettings(frames);
    expect(snap.streaming.has_gemini_key).toBe(true);
    expect(JSON.stringify(snap)).not.toContain('AIzaSECRET');
  });

  it('clear_romanization_cache replies with a drained count', () => {
    const { t, frames } = boot();
    t.send({ v: 1, id: 7, kind: 'req', name: 'clear_romanization_cache' });
    vi.advanceTimersByTime(20);
    const res = frames.find((e) => e.kind === 'res' && e.id === 7)!;
    expect((res.payload as { cleared: number }).cleared).toBeGreaterThan(0);
  });

  it('apply mutates an animations flag and re-pushes', () => {
    const { t, frames } = boot();
    expect(lastSettings(frames).animations.master).toBe(false);
    t.send({
      v: 1,
      kind: 'cmd',
      name: 'apply',
      payload: { change: { group: 'animations', field: 'master', value: true } },
    });
    vi.advanceTimersByTime(20);
    expect(lastSettings(frames).animations.master).toBe(true);
  });

  it('reset_all_settings restores the defaults', () => {
    const { t, frames } = boot();
    t.send({
      v: 1,
      kind: 'cmd',
      name: 'apply',
      payload: { change: { group: 'ui', field: 'album_art', value: false } },
    });
    vi.advanceTimersByTime(20);
    expect(lastSettings(frames).ui.album_art).toBe(false);

    t.send({ v: 1, kind: 'cmd', name: 'reset_all_settings' });
    vi.advanceTimersByTime(20);
    expect(lastSettings(frames).ui.album_art).toBe(true);
  });
});
