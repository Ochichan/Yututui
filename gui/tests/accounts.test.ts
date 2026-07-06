// Accounts wiring (docs/gui/07 §11): the store mirrors the `accounts` snapshot, opens the
// browser on an auth-url push (win:openUrl), and sends the connect/config/setter commands;
// the demo core walks a connect flow (auth url → connected snapshot) and mutates fields.

import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { Client } from '../src/lib/ipc/client';
import {
  AccountsStore,
  type AccountsAuthUrl,
  type AccountsSnapshot,
} from '../src/lib/stores/accounts.svelte';
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
  last(kind: OutEnvelope['kind'], name: string): OutEnvelope {
    const e = [...this.sent].reverse().find((s) => s.kind === kind && s.name === name);
    if (!e) throw new Error(`no ${kind} ${name}`);
    return e;
  }
}

const disconnected: AccountsSnapshot = {
  kind: 'accounts_snapshot',
  lastfm: { connected: false, user: null, scrobbling: false, love_sync: false },
  listenbrainz: { submit: false, has_token: false, custom_url: null },
  spotify: { connected: false, user: null, client_id: null, redirect_port: null },
  scrobble_local: false,
};

describe('AccountsStore', () => {
  it('mirrors the accounts snapshot', () => {
    const t = new MockTransport();
    const store = new AccountsStore(new Client(t));
    const snap: AccountsSnapshot = {
      ...disconnected,
      lastfm: { connected: true, user: 'nyan', scrobbling: true, love_sync: false },
    };
    t.emit({ v: 1, kind: 'event', topic: 'accounts', payload: snap });
    expect(store.model.lastfm.connected).toBe(true);
    expect(store.model.lastfm.user).toBe('nyan');
  });

  it('opens the browser on an auth-url push', () => {
    const t = new MockTransport();
    const store = new AccountsStore(new Client(t));
    const auth: AccountsAuthUrl = { kind: 'accounts_auth_url', service: 'spotify', url: 'https://x/y' };
    t.emit({ v: 1, kind: 'event', topic: 'accounts', payload: auth });
    expect(t.last('win', 'openUrl').payload).toMatchObject({ url: 'https://x/y' });
    // Not a snapshot — the model is untouched by an auth-url frame.
    expect(store.model.lastfm.connected).toBe(false);
  });

  it('connect and setter commands carry the right shape', () => {
    const t = new MockTransport();
    const store = new AccountsStore(new Client(t));
    store.connectLastfm();
    expect(t.last('cmd', 'lastfm_connect')).toBeTruthy();

    store.setLastfmScrobbling(true);
    expect(t.last('cmd', 'account_set').payload).toMatchObject({
      service: 'lastfm',
      field: 'scrobbling',
      value: true,
    });

    store.setScrobbleLocal(true);
    expect(t.last('cmd', 'account_set').payload).toMatchObject({ field: 'scrobble_local', value: true });

    store.configureListenBrainz({ submit: true, token: 'tok' });
    expect(t.last('cmd', 'listen_brainz_configure').payload).toMatchObject({
      submit: true,
      token: 'tok',
    });
  });
});

describe('demo core accounts', () => {
  beforeEach(() => vi.useFakeTimers());
  afterEach(() => vi.useRealTimers());

  function boot() {
    const t = new DemoCoreTransport();
    const frames: InEnvelope[] = [];
    t.onMessage((e) => frames.push(e));
    vi.advanceTimersByTime(200);
    t.send({ v: 1, kind: 'sub', name: 'subscribe', payload: ['accounts'] });
    vi.advanceTimersByTime(50);
    return { t, frames };
  }
  const lastSnap = (frames: InEnvelope[]) =>
    [...frames].reverse().find(
      (e) =>
        e.kind === 'event' &&
        e.topic === 'accounts' &&
        (e.payload as { kind: string }).kind === 'accounts_snapshot',
    )!.payload as AccountsSnapshot;
  const authUrls = (frames: InEnvelope[]) =>
    frames.filter(
      (e) =>
        e.kind === 'event' &&
        e.topic === 'accounts' &&
        (e.payload as { kind: string }).kind === 'accounts_auth_url',
    );

  it('subscribe pushes a disconnected snapshot', () => {
    const { frames } = boot();
    expect(lastSnap(frames).lastfm.connected).toBe(false);
  });

  it('lastfm_connect hands over an auth url then flips connected', () => {
    const { t, frames } = boot();
    t.send({ v: 1, kind: 'cmd', name: 'lastfm_connect', payload: {} });
    vi.advanceTimersByTime(20);
    expect(authUrls(frames).length).toBe(1);
    vi.advanceTimersByTime(300);
    const snap = lastSnap(frames);
    expect(snap.lastfm.connected).toBe(true);
    expect(snap.lastfm.user).toBeTruthy();
  });

  it('account_set and listen_brainz_configure mutate the snapshot', () => {
    const { t, frames } = boot();
    t.send({
      v: 1,
      kind: 'cmd',
      name: 'account_set',
      payload: { service: 'lastfm', field: 'scrobble_local', value: true },
    });
    vi.advanceTimersByTime(20);
    expect(lastSnap(frames).scrobble_local).toBe(true);

    t.send({
      v: 1,
      kind: 'cmd',
      name: 'listen_brainz_configure',
      payload: { submit: true, token: 'abc' },
    });
    vi.advanceTimersByTime(20);
    const snap = lastSnap(frames);
    expect(snap.listenbrainz.submit).toBe(true);
    expect(snap.listenbrainz.has_token).toBe(true);
  });
});
