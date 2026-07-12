// Keymap wiring (docs/gui/05 §8, 07 §8): the store mirrors the pushed keymap read model
// (the src/keymap.rs vocabulary — snake_case ids, config-format chords), resolves chords in
// the correct lookup order, applies optimistic rebinds reconciled on the push, surfaces the
// core's shadow conflict (which does NOT apply the bind), and drives capture. resolveContext
// maps focus to a KeyContext.

import { describe, expect, it } from 'vitest';
import { Client } from '../src/lib/ipc/client';
import { KeymapStore, defaultKeymap, KEY_CONTEXTS } from '../src/lib/stores/keymap.svelte';
import { resolveContext } from '../src/lib/keyboard/dispatcher';
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

function pushKeymap(t: MockTransport): void {
  t.emit({
    v: 1,
    kind: 'event',
    topic: 'settings',
    payload: { kind: 'settings_snapshot', model: { keymap: defaultKeymap() } },
  });
}

const settle = () => new Promise((r) => setTimeout(r, 60));

describe('KeymapStore model + lookup', () => {
  it('mirrors the pushed model and groups every context that has actions', () => {
    const t = new MockTransport();
    const store = new KeymapStore(new Client(t));
    expect(store.model).toBeNull();
    pushKeymap(t);
    // Every context except ai_suggestions carries at least one default_bindings row.
    expect(store.groups.length).toBe(KEY_CONTEXTS.length - 1);
    expect(store.groups.some((g) => g.context === 'ai_suggestions')).toBe(false);
    const player = store.groups.find((g) => g.context === 'player');
    expect(player?.actions.some((a) => a.id === 'toggle_pause')).toBe(true);
  });

  it('resolves chords in order specific → common → global', () => {
    const t = new MockTransport();
    const store = new KeymapStore(new Client(t));
    pushKeymap(t);
    // specific context binding
    expect(store.match('player', 'space')).toBe('toggle_pause');
    // global fallback from any context
    expect(store.match('player', '?')).toBe('toggle_help');
    expect(store.match('library', 'ctrl+h')).toBe('home');
    // common fallback (player has no enter binding of its own)
    expect(store.match('player', 'enter')).toBe('confirm');
    // specific context wins over the fallbacks ('q' is both player.back and common.back)
    expect(store.match('settings', 'q')).toBe('settings_cancel');
    // no binding
    expect(store.match('player', 'ctrl+shift+z')).toBeNull();
  });
});

describe('KeymapStore against the demo core', () => {
  it('rebinds optimistically and reconciles on the push', async () => {
    const client = new Client(new DemoCoreTransport());
    const store = new KeymapStore(client);
    client.sub(['settings']);
    await settle();

    const seekBack = store.actions.find((a) => a.context === 'player' && a.id === 'seek_back')!;
    expect(store.chordFor(seekBack)).toBe('left');

    await store.rebind('player', 'seek_back', 'ctrl+b');
    expect(store.chordFor(seekBack)).toBe('ctrl+b');
    expect(store.match('player', 'ctrl+b')).toBe('seek_back');
  });

  it('a shadow conflict reports inline and does NOT apply the bind', async () => {
    const client = new Client(new DemoCoreTransport());
    const store = new KeymapStore(client);
    client.sub(['settings']);
    await settle();

    // 'space' already belongs to player.toggle_pause — binding it to next_track collides.
    await store.rebind('player', 'next_track', 'space');
    expect(store.conflict?.shadows).toContain('Play / pause');
    // The core did not apply it (and pushed nothing) — the optimistic chord rolled back.
    const next = store.actions.find((a) => a.context === 'player' && a.id === 'next_track')!;
    expect(store.chordFor(next)).toBe('.');
    expect(store.match('player', 'space')).toBe('toggle_pause');
  });

  it('unbinds (wire form: empty string) and resets all back to defaults', async () => {
    const client = new Client(new DemoCoreTransport());
    const store = new KeymapStore(client);
    client.sub(['settings']);
    await settle();

    const next = store.actions.find((a) => a.context === 'player' && a.id === 'next_track')!;
    store.unbind('player', 'next_track');
    await settle();
    expect(store.chordFor(next)).toBe('');
    expect(store.match('player', '.')).toBeNull();
    // The overlay drained on the push — the authoritative '' agrees with the unbind.
    expect(store.model?.bindings['player.next_track']).toBe('');

    store.resetAll();
    await settle();
    expect(store.chordFor(next)).toBe('.');
  });

  it('capture rebinds the target and clears itself', async () => {
    const client = new Client(new DemoCoreTransport());
    const store = new KeymapStore(client);
    client.sub(['settings']);
    await settle();

    store.startCapture('player', 'toggle_shuffle');
    expect(store.capture).toEqual({ context: 'player', action: 'toggle_shuffle' });
    store.applyCapture('ctrl+j');
    expect(store.capture).toBeNull();
    await settle();
    const shuffle = store.actions.find(
      (a) => a.context === 'player' && a.id === 'toggle_shuffle',
    )!;
    expect(store.chordFor(shuffle)).toBe('ctrl+j');
  });
});

describe('resolveContext', () => {
  it('prefers an explicit data-kctx container, else the active view', () => {
    const box = document.createElement('div');
    box.setAttribute('data-kctx', 'search_input');
    const input = document.createElement('input');
    box.appendChild(input);
    document.body.appendChild(box);

    expect(resolveContext('search', input)).toBe('search_input');
    expect(resolveContext('search', null)).toBe('search_results');
    expect(resolveContext('now', null)).toBe('player');
    expect(resolveContext('settings', null)).toBe('settings');

    document.body.removeChild(box);
  });
});
