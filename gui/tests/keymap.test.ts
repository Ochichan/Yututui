// Keymap wiring (docs/gui/05 §8, 07 §8): the store mirrors the pushed keymap read model,
// resolves chords in the correct lookup order, applies optimistic rebinds reconciled on the
// push, surfaces the core's shadow conflict, and drives capture. resolveContext maps focus to
// a KeyContext.

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
  it('mirrors the pushed model and groups all 11 contexts', () => {
    const t = new MockTransport();
    const store = new KeymapStore(new Client(t));
    expect(store.model).toBeNull();
    pushKeymap(t);
    expect(store.groups.length).toBe(KEY_CONTEXTS.length);
    const player = store.groups.find((g) => g.context === 'Player');
    expect(player?.actions.some((a) => a.id === 'play_pause')).toBe(true);
  });

  it('resolves chords in order specific → Common → Global', () => {
    const t = new MockTransport();
    const store = new KeymapStore(new Client(t));
    pushKeymap(t);
    // specific context binding
    expect(store.match('Player', 'Space')).toBe('play_pause');
    // Global fallback from any context
    expect(store.match('Player', '?')).toBe('help');
    expect(store.match('Library', '1')).toBe('view_now');
    // Common fallback
    expect(store.match('Player', 'q')).toBe('toggle_queue');
    // no binding
    expect(store.match('Player', 'Ctrl+Shift+z')).toBeNull();
  });
});

describe('KeymapStore against the demo core', () => {
  it('rebinds optimistically and reconciles on the push', async () => {
    const client = new Client(new DemoCoreTransport());
    const store = new KeymapStore(client);
    client.sub(['settings']);
    await settle();

    const seekBack = store.actions.find((a) => a.context === 'Player' && a.id === 'seek_back')!;
    expect(store.chordFor(seekBack)).toBe('Left');

    await store.rebind('Player', 'seek_back', 'Ctrl+b');
    expect(store.chordFor(seekBack)).toBe('Ctrl+b');
    expect(store.match('Player', 'Ctrl+b')).toBe('seek_back');
  });

  it('surfaces the core-side shadow conflict from the bind reply', async () => {
    const client = new Client(new DemoCoreTransport());
    const store = new KeymapStore(client);
    client.sub(['settings']);
    await settle();

    // 'Space' already belongs to Player.play_pause — binding it to next collides.
    await store.rebind('Player', 'next', 'Space');
    expect(store.conflict?.shadows).toBe('play_pause');
  });

  it('unbinds and resets all back to defaults', async () => {
    const client = new Client(new DemoCoreTransport());
    const store = new KeymapStore(client);
    client.sub(['settings']);
    await settle();

    const next = store.actions.find((a) => a.context === 'Player' && a.id === 'next')!;
    store.unbind('Player', 'next');
    await settle();
    expect(store.chordFor(next)).toBe('');
    expect(store.match('Player', 'n')).toBeNull();

    store.resetAll();
    await settle();
    expect(store.chordFor(next)).toBe('n');
  });

  it('capture rebinds the target and clears itself', async () => {
    const client = new Client(new DemoCoreTransport());
    const store = new KeymapStore(client);
    client.sub(['settings']);
    await settle();

    store.startCapture('Player', 'toggle_shuffle');
    expect(store.capture).toEqual({ context: 'Player', action: 'toggle_shuffle' });
    store.applyCapture('Ctrl+h');
    expect(store.capture).toBeNull();
    await settle();
    const shuffle = store.actions.find((a) => a.id === 'toggle_shuffle')!;
    expect(store.chordFor(shuffle)).toBe('Ctrl+h');
  });
});

describe('resolveContext', () => {
  it('prefers an explicit data-kctx container, else the active view', () => {
    const box = document.createElement('div');
    box.setAttribute('data-kctx', 'SearchInput');
    const input = document.createElement('input');
    box.appendChild(input);
    document.body.appendChild(box);

    expect(resolveContext('search', input)).toBe('SearchInput');
    expect(resolveContext('search', null)).toBe('SearchResults');
    expect(resolveContext('now', null)).toBe('Player');
    expect(resolveContext('settings', null)).toBe('Settings');

    document.body.removeChild(box);
  });
});
