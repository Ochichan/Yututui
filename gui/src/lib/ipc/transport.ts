// The transport is the thin boundary between the frontend and the Rust shell (docs/gui/05
// §4.3). It (de)serializes envelopes; the Client owns all correlation/demux logic.

import type { InEnvelope, OutEnvelope } from './envelope';

export interface Transport {
  send(env: OutEnvelope): void;
  onMessage(cb: (env: InEnvelope) => void): void;
  /** True against the real Rust shell (window.ipc present); false in a plain browser. */
  readonly live: boolean;
}

interface WryIpc {
  postMessage(msg: string): void;
}
declare global {
  interface Window {
    ipc?: WryIpc;
    __ytm?: { receive(json: string): void; persist?(): void };
  }
}

/**
 * Real transport: wry injects `window.ipc.postMessage`; the shell (bridge.rs) pushes frames
 * by calling `window.__ytm.receive(jsonLine)` via evaluate_script on the loop thread.
 */
export class WryTransport implements Transport {
  readonly live = true;
  #cb: ((env: InEnvelope) => void) | null = null;

  constructor() {
    window.__ytm = {
      ...(window.__ytm ?? {}),
      receive: (json: string) => {
        try {
          this.#cb?.(JSON.parse(json) as InEnvelope);
        } catch (e) {
          console.error('[ipc] dropped malformed inbound frame', e);
        }
      },
    };
  }

  send(env: OutEnvelope): void {
    window.ipc?.postMessage(JSON.stringify(env));
  }

  onMessage(cb: (env: InEnvelope) => void): void {
    this.#cb = cb;
  }
}

/** A scripted fake for the browser/dev/E2E vehicle: no Rust, replays canned frames. */
export interface FakeScript {
  /** Frames pushed after onMessage registers, as [delayMs, envelope]. */
  initial?: Array<[number, InEnvelope]>;
  /** Optional responder to outbound frames (e.g. answer `req ping` with `res pong`). */
  respond?(env: OutEnvelope): InEnvelope | void;
}

export class FakeTransport implements Transport {
  readonly live = false;
  #cb: ((env: InEnvelope) => void) | null = null;
  readonly #script: FakeScript;

  constructor(script: FakeScript = defaultFakeScript()) {
    this.#script = script;
  }

  send(env: OutEnvelope): void {
    const reply = this.#script.respond?.(env);
    if (reply) queueMicrotask(() => this.#emit(reply));
  }

  onMessage(cb: (env: InEnvelope) => void): void {
    this.#cb = cb;
    for (const [delay, env] of this.#script.initial ?? []) {
      setTimeout(() => this.#emit(env), delay);
    }
  }

  #emit(env: InEnvelope): void {
    this.#cb?.(env);
  }
}

/** Default browser script: connect → online, and echo `req ping` → `res pong`. */
export function defaultFakeScript(): FakeScript {
  return {
    initial: [
      [0, { v: 1, kind: 'conn', payload: { state: 'connecting' } }],
      [
        40,
        {
          v: 1,
          kind: 'conn',
          payload: {
            state: 'online',
            coreVersion: 'fakecore',
            protocolVersion: 8,
            capabilities: ['events-v8'],
            ownerMode: 'standalone_tui',
          },
        },
      ],
    ],
    respond(env) {
      if (env.kind === 'req' && env.name === 'ping') {
        return { v: 1, id: env.id, kind: 'res', payload: 'pong' };
      }
      return undefined;
    },
  };
}
