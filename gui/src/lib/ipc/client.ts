// The typed IPC surface (docs/gui/05 §4.2): acknowledged mutations, correlated requests,
// latest-state topic subscriptions, native window ops, and reconnect driven by `conn` frames.

import type { ConnPayload, ConnState, InEnvelope } from './envelope';
import type { Transport } from './transport';
import type { Topic } from '../../generated/protocol/Topic';

const REQ_TIMEOUT_MS = 10_000;
const MAX_PENDING = 256;

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

export type MutationResult<T = unknown> =
  { ok: true; payload: T } | { ok: false; name: string; reason: string };

export interface MutationFailure {
  name: string;
  reason: string;
}

interface Pending {
  kind: 'cmd' | 'req';
  resolve: (v: unknown) => void;
  reject: (e: Error) => void;
  timer: ReturnType<typeof setTimeout>;
}

export class Client {
  readonly #transport: Transport;
  #nextId = 1;
  readonly #pageId: string;
  readonly #pending = new Map<number, Pending>();
  readonly #handlers = new Map<string, Set<TopicHandler>>();
  readonly #connListeners = new Set<(info: ConnInfo) => void>();
  readonly #mutationFailureListeners = new Set<(failure: MutationFailure) => void>();
  readonly #desiredTopics = new Set<Topic>();
  #replaySubscriptions = false;

  conn: ConnInfo = {
    state: 'connecting',
    coreVersion: null,
    protocolVersion: null,
    capabilities: [],
    ownerMode: null,
    reason: null,
  };

  constructor(transport: Transport, pageId = pageNamespace()) {
    this.#transport = transport;
    this.#pageId = pageId;
    transport.onMessage((env) => this.#receive(env));
  }

  /**
   * A user mutation with a bounded correlated acknowledgement. Failures resolve as data so a
   * caller that does not need local rollback cannot create an unhandled rejection; every failure
   * is also broadcast to the global status/toast surface.
   *
   * Mutations are deliberately not retried here. A disconnect after owner admission is
   * ambiguous, and replaying arbitrary user actions would be less safe than an explicit error.
   */
  async cmd<T = unknown>(name: string, payload?: unknown): Promise<MutationResult<T>> {
    try {
      return { ok: true, payload: await this.#correlated<T>('cmd', name, payload) };
    } catch (error) {
      const failure = { name, reason: errorReason(error) };
      for (const cb of this.#mutationFailureListeners) {
        try {
          cb(failure);
        } catch (listenerError) {
          console.error('[ipc] mutation failure listener threw', listenerError);
        }
      }
      return { ok: false, ...failure };
    }
  }

  /** Correlated request/response with a 10 s timeout. */
  req<T = unknown>(name: string, payload?: unknown): Promise<T> {
    return this.#correlated<T>('req', name, payload);
  }

  #correlated<T>(kind: 'cmd' | 'req', name: string, payload?: unknown): Promise<T> {
    if (this.#pending.size >= MAX_PENDING) {
      return Promise.reject(new IpcError('busy', 'IPC request queue is full'));
    }
    const id = this.#nextId++;
    return new Promise<T>((resolve, reject) => {
      const timer = setTimeout(() => {
        this.#pending.delete(id);
        reject(
          kind === 'cmd'
            ? new IpcError('confirmation_lost', `confirmation for mutation "${name}" was lost`)
            : new IpcError('timeout', `request "${name}" timed out`),
        );
      }, REQ_TIMEOUT_MS);
      this.#pending.set(id, {
        kind,
        resolve: resolve as (v: unknown) => void,
        reject,
        timer,
      });
      try {
        this.#transport.send({
          v: 1,
          id,
          page_id: this.#pageId,
          request_id: `gui:${this.#pageId}:${id}`,
          kind,
          name,
          payload,
        });
      } catch (error) {
        // A native bridge can fail synchronously before it owns the envelope. Do not leave a
        // phantom pending slot and timer behind for an action that was never admitted.
        clearTimeout(timer);
        this.#pending.delete(id);
        reject(error);
      }
    });
  }

  sub(topics: Topic[]): void {
    const added = topics.filter((topic) => {
      if (this.#desiredTopics.has(topic)) return false;
      this.#desiredTopics.add(topic);
      return true;
    });
    if (added.length) this.#sendTopics('sub', added);
  }

  unsub(topics: Topic[]): void {
    const removed = topics.filter((topic) => this.#desiredTopics.delete(topic));
    if (removed.length) this.#sendTopics('unsub', removed);
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

  /** Observe acknowledged mutation failures (the global ToastStore is the normal consumer). */
  onMutationFailure(cb: (failure: MutationFailure) => void): Unsub {
    this.#mutationFailureListeners.add(cb);
    return () => this.#mutationFailureListeners.delete(cb);
  }

  /** Native window op (drag/hide/openDevtools/copyText/openUrl) — handled in the tao loop. */
  win(name: string, payload?: unknown): void {
    this.#transport.send({ v: 1, page_id: this.#pageId, kind: 'win', name, payload });
  }

  #receive(env: InEnvelope): void {
    switch (env.kind) {
      case 'res':
      case 'err': {
        // The native gateway can outlive and replace a WebView. An older page's in-flight reply
        // may therefore arrive after this page has reused the same small numeric id.
        if (env.page_id != null && env.page_id !== this.#pageId) return;
        if (env.id == null) return;
        const p = this.#pending.get(env.id);
        if (!p) return;
        this.#pending.delete(env.id);
        clearTimeout(p.timer);
        if (env.kind === 'res') {
          p.resolve(env.payload);
        } else {
          const reason = errReason(env.payload);
          // Compatible protocol-v8 owners used `timeout` for this ambiguous condition. The
          // operation kind is known locally, so never turn a lost mutation reply into rejection.
          p.reject(
            new IpcError(p.kind === 'cmd' && reason === 'timeout' ? 'confirmation_lost' : reason),
          );
        }
        return;
      }
      case 'event': {
        // The native gateway can outlive and replace a WebView. Page-scoped pushes from an
        // earlier page must not reach handlers in the replacement page, even when both pages
        // reused the same search ticket.
        if (env.page_id != null && env.page_id !== this.#pageId) return;
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
      this.#replaySubscriptions = true;
      for (const [id, pend] of this.#pending) {
        clearTimeout(pend.timer);
        pend.reject(
          pend.kind === 'cmd'
            ? new IpcError('confirmation_lost', 'mutation confirmation lost with connection')
            : new IpcError(p.reason ?? 'offline', 'connection lost'),
        );
        this.#pending.delete(id);
      }
    }
    if (p.state === 'online' && this.#replaySubscriptions) {
      this.#replaySubscriptions = false;
      this.#sendTopics('sub', [...this.#desiredTopics]);
    }
    for (const cb of this.#connListeners) cb(this.conn);
  }

  #sendTopics(kind: 'sub' | 'unsub', topics: Topic[]): void {
    if (!topics.length) return;
    this.#transport.send({
      v: 1,
      page_id: this.#pageId,
      kind,
      name: kind === 'sub' ? 'subscribe' : 'unsubscribe',
      payload: topics,
    });
  }
}

class IpcError extends Error {
  constructor(
    readonly reason: string,
    message = reason,
  ) {
    super(message);
  }
}

function errReason(payload: unknown): string {
  if (payload && typeof payload === 'object' && 'reason' in payload) {
    return String((payload as { reason: unknown }).reason);
  }
  return typeof payload === 'string' ? payload : 'error';
}

function errorReason(error: unknown): string {
  return error instanceof IpcError
    ? error.reason
    : error instanceof Error
      ? error.message
      : 'error';
}

function pageNamespace(): string {
  const words = new Uint32Array(4);
  if (globalThis.crypto?.getRandomValues) {
    globalThis.crypto.getRandomValues(words);
    return [...words].map((word) => word.toString(16).padStart(8, '0')).join('');
  }
  // Old embedded WebViews without Web Crypto still get a page-lifetime collision-resistant id.
  return `${Date.now().toString(36)}-${Math.random().toString(36).slice(2)}`;
}
