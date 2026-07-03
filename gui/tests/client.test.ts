import { describe, expect, it, vi } from 'vitest';
import { Client } from '../src/lib/ipc/client';
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

  it('rejects on an err frame using the machine reason', async () => {
    const t = new MockTransport();
    const c = new Client(t);
    const p = c.req('next');
    t.emit({ v: 1, id: t.last().id, kind: 'err', payload: { reason: 'queue_empty' } });
    await expect(p).rejects.toThrow('queue_empty');
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
});
