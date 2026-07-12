import { describe, expect, it, vi } from 'vitest';
import { Client } from '../src/lib/ipc/client';
import type { Transport } from '../src/lib/ipc/transport';
import type { InEnvelope, OutEnvelope } from '../src/lib/ipc/envelope';
import { ToastStore } from '../src/lib/stores/toasts.svelte';

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
  last(): OutEnvelope {
    const e = this.sent.at(-1);
    if (!e) throw new Error('no frame sent');
    return e;
  }
}

describe('Client', () => {
  it('correlates req/res by id', async () => {
    const t = new MockTransport();
    const c = new Client(t);
    const p = c.req<string>('ping');
    const sent = t.last();
    expect(sent.kind).toBe('req');
    expect(sent.name).toBe('ping');
    t.emit({ v: 1, id: sent.id, kind: 'res', payload: 'pong' });
    await expect(p).resolves.toBe('pong');
  });

  it('ignores an in-flight reply from a replaced page that reused the numeric id', async () => {
    vi.useFakeTimers();
    try {
      const t = new MockTransport();
      const oldClient = new Client(t, 'page-a');
      const oldRequest = oldClient.req<string>('status');
      const oldTimeout = expect(oldRequest).rejects.toThrow('timed out');
      const oldFrame = t.last();

      // Rebuilding the WebView constructs a fresh Client whose small correlation ids restart.
      const replacement = new Client(t, 'page-b');
      const currentRequest = replacement.req<string>('status');
      const currentFrame = t.last();
      expect(oldFrame.id).toBe(1);
      expect(currentFrame.id).toBe(1);

      let currentSettled = false;
      void currentRequest.finally(() => {
        currentSettled = true;
      });
      t.emit({
        v: 1,
        id: oldFrame.id,
        page_id: oldFrame.page_id,
        kind: 'res',
        payload: 'stale',
      });
      await Promise.resolve();
      expect(currentSettled).toBe(false);

      t.emit({
        v: 1,
        id: currentFrame.id,
        page_id: currentFrame.page_id,
        kind: 'res',
        payload: 'current',
      });
      await expect(currentRequest).resolves.toBe('current');

      await vi.advanceTimersByTimeAsync(10_000);
      await oldTimeout;
    } finally {
      vi.useRealTimers();
    }
  });

  it('uses a new stable identity when a replacement page repeats a completed request id', async () => {
    const t = new MockTransport();
    const oldClient = new Client(t, 'page-a');
    const oldRequest = oldClient.req('status');
    const oldFrame = t.last();
    t.emit({
      v: 1,
      id: oldFrame.id,
      page_id: oldFrame.page_id,
      kind: 'res',
      payload: { ok: true },
    });
    await oldRequest;

    const replacement = new Client(t, 'page-b');
    const currentRequest = replacement.req('status');
    const currentFrame = t.last();
    expect(currentFrame.id).toBe(oldFrame.id);
    expect(oldFrame).toMatchObject({
      page_id: 'page-a',
      request_id: 'gui:page-a:1',
    });
    expect(currentFrame).toMatchObject({
      page_id: 'page-b',
      request_id: 'gui:page-b:1',
    });
    expect(currentFrame.request_id).not.toBe(oldFrame.request_id);

    t.emit({
      v: 1,
      id: currentFrame.id,
      page_id: currentFrame.page_id,
      kind: 'res',
      payload: { ok: true },
    });
    await currentRequest;
  });

  it('rejects on an err frame using the machine reason', async () => {
    const t = new MockTransport();
    const c = new Client(t);
    const p = c.req('next');
    t.emit({ v: 1, id: t.last().id, kind: 'err', payload: { reason: 'queue_empty' } });
    await expect(p).rejects.toThrow('queue_empty');
  });

  it('acknowledges every mutation and surfaces a rejected action in the global toast store', async () => {
    const t = new MockTransport();
    const c = new Client(t);
    const toasts = new ToastStore();
    toasts.attach(c);

    const result = c.cmd('next');
    const sent = t.last();
    expect(sent).toMatchObject({ kind: 'cmd', name: 'next' });
    expect(sent.id).toEqual(expect.any(Number));
    t.emit({ v: 1, id: sent.id, kind: 'err', payload: { reason: 'busy' } });

    await expect(result).resolves.toEqual({ ok: false, name: 'next', reason: 'busy' });
    expect(toasts.toasts.map((toast) => toast.text)).toContain(
      'Player is busy. The action was not applied; try again.',
    );
  });

  it('reports a disconnected in-flight mutation as confirmation lost', async () => {
    const t = new MockTransport();
    const c = new Client(t);
    const toasts = new ToastStore();
    toasts.attach(c);

    const result = c.cmd('next');
    t.emit({ v: 1, kind: 'conn', payload: { state: 'offline', reason: 'disconnected' } });

    await expect(result).resolves.toEqual({
      ok: false,
      name: 'next',
      reason: 'confirmation_lost',
    });
    expect(toasts.toasts.map((toast) => toast.text)).toContain(
      'The action may have been applied. Check the current state before retrying.',
    );
  });

  it('keeps a definite pre-admission rejection distinct from confirmation loss', async () => {
    const t = new MockTransport();
    const c = new Client(t);
    const toasts = new ToastStore();
    toasts.attach(c);

    const result = c.cmd('next');
    t.emit({ v: 1, id: t.last().id, kind: 'err', payload: { reason: 'offline' } });

    await expect(result).resolves.toEqual({ ok: false, name: 'next', reason: 'offline' });
    expect(toasts.toasts.map((toast) => toast.text)).toContain(
      'Player is offline. The action was not applied.',
    );
  });

  it('classifies a lost mutation reply timeout as confirmation lost', async () => {
    vi.useFakeTimers();
    try {
      const t = new MockTransport();
      const c = new Client(t);
      const result = c.cmd('next');
      await vi.advanceTimersByTimeAsync(10_000);
      await expect(result).resolves.toEqual({
        ok: false,
        name: 'next',
        reason: 'confirmation_lost',
      });

      const compatibleCoreResult = c.cmd('prev');
      t.emit({ v: 1, id: t.last().id, kind: 'err', payload: { reason: 'timeout' } });
      await expect(compatibleCoreResult).resolves.toEqual({
        ok: false,
        name: 'prev',
        reason: 'confirmation_lost',
      });
    } finally {
      vi.useRealTimers();
    }
  });

  it('times out after 10s', async () => {
    vi.useFakeTimers();
    try {
      const t = new MockTransport();
      const c = new Client(t);
      const p = c.req('slow');
      const assertion = expect(p).rejects.toThrow('timed out');
      await vi.advanceTimersByTimeAsync(10_000);
      await assertion;
    } finally {
      vi.useRealTimers();
    }
  });

  it('cleans up the pending slot and timer when transport send throws synchronously', async () => {
    vi.useFakeTimers();
    try {
      const t = new MockTransport();
      vi.spyOn(t, 'send').mockImplementation(() => {
        throw new Error('bridge unavailable');
      });
      const c = new Client(t);

      await expect(c.req('never-admitted')).rejects.toThrow('bridge unavailable');
      expect(vi.getTimerCount()).toBe(0);

      // A cleaned slot admits another attempt instead of accumulating toward MAX_PENDING.
      await expect(c.req('second-attempt')).rejects.toThrow('bridge unavailable');
      expect(t.send).toHaveBeenCalledTimes(2);
      expect(vi.getTimerCount()).toBe(0);
    } finally {
      vi.useRealTimers();
    }
  });

  it('drives connection state from conn frames', () => {
    const t = new MockTransport();
    const c = new Client(t);
    const seen: string[] = [];
    c.onConn((i) => seen.push(i.state));
    t.emit({ v: 1, kind: 'conn', payload: { state: 'online', capabilities: ['events-v8'] } });
    expect(c.conn.state).toBe('online');
    expect(c.conn.capabilities).toContain('events-v8');
    expect(seen).toEqual(['connecting', 'online']);
  });

  it('rejects in-flight requests when the connection goes offline', async () => {
    const t = new MockTransport();
    const c = new Client(t);
    const p = c.req('x');
    t.emit({ v: 1, kind: 'conn', payload: { state: 'offline' } });
    await expect(p).rejects.toThrow('connection lost');
  });

  it('demuxes topic events to on() handlers and stops after unsub', () => {
    const t = new MockTransport();
    const c = new Client(t);
    const got: unknown[] = [];
    const unsub = c.on('player', (p) => got.push(p));
    t.emit({ v: 1, kind: 'event', topic: 'player', payload: { hi: 1 } });
    unsub();
    t.emit({ v: 1, kind: 'event', topic: 'player', payload: { hi: 2 } });
    expect(got).toEqual([{ hi: 1 }]);
  });

  it('drops a page-scoped event from a replaced page before topic demux', () => {
    const t = new MockTransport();
    const c = new Client(t, 'page-b');
    const got: unknown[] = [];
    c.on('search', (payload) => got.push(payload));

    t.emit({
      v: 1,
      page_id: 'page-a',
      kind: 'event',
      topic: 'search',
      payload: { kind: 'search_completed', ticket: 1, query: 'stale' },
    });
    t.emit({
      v: 1,
      page_id: 'page-b',
      kind: 'event',
      topic: 'search',
      payload: { kind: 'search_completed', ticket: 1, query: 'current' },
    });
    // Legacy, unscoped pushes remain additive-compatible.
    t.emit({
      v: 1,
      kind: 'event',
      topic: 'search',
      payload: { kind: 'search_completed', ticket: 2, query: 'legacy' },
    });

    expect(got).toEqual([
      { kind: 'search_completed', ticket: 1, query: 'current' },
      { kind: 'search_completed', ticket: 2, query: 'legacy' },
    ]);
  });

  it('coalesces desired topics and replays the latest set after reconnect', () => {
    const t = new MockTransport();
    const c = new Client(t);
    c.sub(['player', 'queue']);
    c.sub(['queue']); // duplicate declarations do not add traffic
    c.unsub(['queue']);
    expect(t.sent).toHaveLength(2);

    t.emit({ v: 1, kind: 'conn', payload: { state: 'offline', reason: 'disconnected' } });
    c.sub(['settings']); // recorded even while offline
    t.emit({ v: 1, kind: 'conn', payload: { state: 'connecting' } });
    t.emit({ v: 1, kind: 'conn', payload: { state: 'online' } });

    expect(t.last()).toMatchObject({
      kind: 'sub',
      name: 'subscribe',
      payload: ['player', 'settings'],
    });
  });
});
