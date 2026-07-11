// The typed IPC surface (docs/gui/05 §4.2): fire-and-forget cmd, correlated req, topic
// sub/on demux, native window ops, and the reconnect state machine driven by `conn` frames.

import type { ConnPayload, ConnState, InEnvelope } from './envelope';
import type { Transport } from './transport';
import type { Topic } from '../../generated/protocol/Topic';

const REQ_TIMEOUT_MS = 10_000;

export interface ConnInfo {
  state: ConnState;
  coreVersion: string | null;
  protocolVersion: number | null;
  capabilities: string[];
  ownerMode: string | null;
  reason: string | null;
}

export type Unsub = () => void;
type TopicHandler = (payload: unknown) => void;

interface Pending {
  resolve: (v: unknown) => void;
  reject: (e: Error) => void;
  timer: ReturnType<typeof setTimeout>;
}

export class Client {
  readonly #transport: Transport;
  #nextId = 1;
  readonly #pending = new Map<number, Pending>();
  readonly #handlers = new Map<string, Set<TopicHandler>>();
  readonly #connListeners = new Set<(info: ConnInfo) => void>();

  conn: ConnInfo = {
    state: 'connecting',
    coreVersion: null,
    protocolVersion: null,
    capabilities: [],
    ownerMode: null,
    reason: null,
  };

  constructor(transport: Transport) {
    this.#transport = transport;
    transport.onMessage((env) => this.#receive(env));
  }

  /** Fire-and-forget user action; truth arrives via a subsequent `event` push. */
  cmd(name: string, payload?: unknown): void {
    this.#transport.send({ v: 1, kind: 'cmd', name, payload });
  }

  /** Correlated request/response with a 10 s timeout. */
  req<T = unknown>(name: string, payload?: unknown): Promise<T> {
    const id = this.#nextId++;
    return new Promise<T>((resolve, reject) => {
      const timer = setTimeout(() => {
        this.#pending.delete(id);
        reject(new Error(`request "${name}" timed out`));
      }, REQ_TIMEOUT_MS);
      this.#pending.set(id, {
        resolve: resolve as (v: unknown) => void,
        reject,
        timer,
      });
      this.#transport.send({ v: 1, id, kind: 'req', name, payload });
    });
  }

  sub(topics: Topic[]): void {
    if (topics.length) {
      this.#transport.send({ v: 1, kind: 'sub', name: 'subscribe', payload: topics });
    }
  }

  unsub(topics: Topic[]): void {
    if (topics.length) {
      this.#transport.send({ v: 1, kind: 'unsub', name: 'unsubscribe', payload: topics });
    }
  }

  /** Force a fresh snapshot while preserving the long-lived subscription. The native gateway
   * serializes unsubscribe+subscribe as one command so backpressure cannot strand the topic. */
  refresh(topics: Topic[]): void {
    if (topics.length) {
      this.#transport.send({ v: 1, kind: 'sub', name: 'refresh', payload: topics });
    }
  }

  /** Subscribe to a topic's decoded payloads. Returns an unsubscribe fn. */
  on(topic: Topic, cb: TopicHandler): Unsub {
    let set = this.#handlers.get(topic);
    if (!set) {
      set = new Set();
      this.#handlers.set(topic, set);
    }
    set.add(cb);
    return () => set.delete(cb);
  }

  /** Observe connection-state changes; fires immediately with the current state. */
  onConn(cb: (info: ConnInfo) => void): Unsub {
    this.#connListeners.add(cb);
    cb(this.conn);
    return () => this.#connListeners.delete(cb);
  }

  /** Native window op (drag/hide/frontendReady/copyText/openUrl) — handled in the tao loop. */
  win(name: string, payload?: unknown): void {
    this.#transport.send({ v: 1, kind: 'win', name, payload });
  }

  #receive(env: InEnvelope): void {
    switch (env.kind) {
      case 'res':
      case 'err': {
        if (env.id == null) return;
        const p = this.#pending.get(env.id);
        if (!p) return;
        this.#pending.delete(env.id);
        clearTimeout(p.timer);
        if (env.kind === 'res') p.resolve(env.payload);
        else p.reject(new Error(errReason(env.payload)));
        return;
      }
      case 'event': {
        if (!env.topic) return;
        const set = this.#handlers.get(env.topic);
        if (set) for (const cb of set) cb(env.payload);
        return;
      }
      case 'conn': {
        this.#applyConn(env.payload as ConnPayload);
        return;
      }
    }
  }

  #applyConn(p: ConnPayload): void {
    this.conn = {
      state: p.state,
      coreVersion: p.coreVersion ?? this.conn.coreVersion,
      protocolVersion: p.protocolVersion ?? this.conn.protocolVersion,
      capabilities: p.capabilities ?? this.conn.capabilities,
      ownerMode: p.ownerMode ?? this.conn.ownerMode,
      reason: p.reason ?? null,
    };
    // A hard drop rejects in-flight requests so callers don't hang for the full timeout.
    if (p.state === 'offline') {
      for (const [id, pend] of this.#pending) {
        clearTimeout(pend.timer);
        pend.reject(new Error('connection lost'));
        this.#pending.delete(id);
      }
    }
    for (const cb of this.#connListeners) cb(this.conn);
  }
}

function errReason(payload: unknown): string {
  if (payload && typeof payload === 'object' && 'reason' in payload) {
    return String((payload as { reason: unknown }).reason);
  }
  return typeof payload === 'string' ? payload : 'error';
}
